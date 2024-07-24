// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use databend_common_catalog::catalog::Catalog;
use databend_common_catalog::lock::LockTableOption;
use databend_common_catalog::plan::Filters;
use databend_common_catalog::plan::PushDownInfo;
use databend_common_catalog::table::TableExt;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::type_check::check_function;
use databend_common_functions::BUILTIN_FUNCTIONS;
use databend_common_meta_app::schema::TableInfo;
use databend_common_sql::executor::physical_plans::Exchange;
use databend_common_sql::executor::physical_plans::FragmentKind;
use databend_common_sql::executor::physical_plans::ReclusterSink;
use databend_common_sql::executor::physical_plans::ReclusterSource;
use databend_common_sql::executor::PhysicalPlan;
use databend_common_storages_fuse::operations::ReclusterTasks;
use databend_common_storages_fuse::FuseTable;
use databend_storages_common_table_meta::meta::TableSnapshot;
use log::error;
use log::warn;

use crate::interpreters::Interpreter;
use crate::interpreters::InterpreterClusteringHistory;
use crate::interpreters::OptimizeTableInterpreter;
use crate::pipelines::executor::ExecutorSettings;
use crate::pipelines::executor::PipelineCompleteExecutor;
use crate::pipelines::PipelineBuildResult;
use crate::schedulers::build_query_pipeline_without_render_result_set;
use crate::sessions::QueryContext;
use crate::sessions::TableContext;
use crate::sql::executor::cast_expr_to_non_null_boolean;
use crate::sql::plans::ReclusterTablePlan;

pub struct ReclusterTableInterpreter {
    ctx: Arc<QueryContext>,
    plan: ReclusterTablePlan,
}

impl ReclusterTableInterpreter {
    pub fn try_create(ctx: Arc<QueryContext>, plan: ReclusterTablePlan) -> Result<Self> {
        Ok(Self { ctx, plan })
    }
}

#[async_trait::async_trait]
impl Interpreter for ReclusterTableInterpreter {
    fn name(&self) -> &str {
        "ReclusterTableInterpreter"
    }

    fn is_ddl(&self) -> bool {
        true
    }

    #[async_backtrace::framed]
    async fn execute2(&self) -> Result<PipelineBuildResult> {
        let plan = &self.plan;
        let ctx = self.ctx.clone();
        let recluster_timeout_secs = ctx.get_settings().get_recluster_timeout_secs()?;

        // Build extras via push down scalar
        let extras = if let Some(scalar) = &plan.push_downs {
            // prepare the filter expression
            let filter = cast_expr_to_non_null_boolean(
                scalar
                    .as_expr()?
                    .project_column_ref(|col| col.column_name.clone()),
            )?;
            // prepare the inverse filter expression
            let inverted_filter =
                check_function(None, "not", &[], &[filter.clone()], &BUILTIN_FUNCTIONS)?;

            let filters = Filters {
                filter: filter.as_remote_expr(),
                inverted_filter: inverted_filter.as_remote_expr(),
            };

            Some(PushDownInfo {
                filters: Some(filters),
                ..PushDownInfo::default()
            })
        } else {
            None
        };

        let mut times = 0;
        let mut block_count = 0;
        let start = SystemTime::now();
        let timeout = Duration::from_secs(recluster_timeout_secs);
        let catalog = self.ctx.get_catalog(&plan.catalog).await?;
        loop {
            if let Err(err) = ctx.check_aborting() {
                error!(
                    "execution of recluster statement aborted. server is shutting down or the query was killed. table: {}",
                    plan.table,
                );
                return Err(err);
            }

            let res = self
                .execute_recluster(catalog.clone(), extras.clone(), &mut block_count)
                .await;

            match res {
                Ok(is_break) => {
                    if is_break {
                        break;
                    }
                }
                Err(e) => {
                    if plan.is_final
                        && matches!(
                            e.code(),
                            ErrorCode::TABLE_LOCK_EXPIRED
                                | ErrorCode::TABLE_ALREADY_LOCKED
                                | ErrorCode::TABLE_VERSION_MISMATCHED
                                | ErrorCode::UNRESOLVABLE_CONFLICT
                        )
                    {
                        warn!("Execute recluster error: {:?}", e);
                    } else {
                        return Err(e);
                    }
                }
            }

            let elapsed_time = SystemTime::now().duration_since(start).unwrap();
            times += 1;
            // Status.
            {
                let status = format!(
                    "recluster: run recluster tasks:{} times, cost:{:?}",
                    times, elapsed_time
                );
                ctx.set_status_info(&status);
            }

            if !plan.is_final {
                break;
            }

            if elapsed_time >= timeout {
                warn!(
                    "Recluster stopped because the runtime was over {:?}",
                    timeout
                );
                break;
            }
        }

        if block_count != 0 {
            InterpreterClusteringHistory::write_log(
                &ctx,
                start,
                &plan.database,
                &plan.table,
                block_count,
            )?;
        }

        Ok(PipelineBuildResult::create())
    }
}

impl ReclusterTableInterpreter {
    async fn execute_recluster(
        &self,
        catalog: Arc<dyn Catalog>,
        extras: Option<PushDownInfo>,
        block_count: &mut u64,
    ) -> Result<bool> {
        let tenant = self.ctx.get_tenant();
        // try add lock table.
        let lock_guard = self
            .ctx
            .clone()
            .acquire_table_lock(
                &self.plan.catalog,
                &self.plan.database,
                &self.plan.table,
                &LockTableOption::LockWithRetry,
            )
            .await?;

        let table = catalog
            .get_table(&tenant, &self.plan.database, &self.plan.table)
            .await?;
        // check mutability
        table.check_mutable()?;

        let fuse_table = FuseTable::try_from_table(table.as_ref())?;
        let mutator = fuse_table
            .build_recluster_mutator(self.ctx.clone(), extras.clone(), self.plan.limit)
            .await?;
        if mutator.is_none() {
            return Ok(true);
        };

        let mutator = mutator.unwrap();
        if mutator.tasks.is_empty() {
            return Ok(true);
        }
        let is_distributed = mutator.is_distributed();
        *block_count += mutator.recluster_blocks_count;
        let physical_plan = build_recluster_physical_plan(
            mutator.tasks,
            table.get_table_info().clone(),
            mutator.snapshot,
            is_distributed,
        )?;

        let mut build_res =
            build_query_pipeline_without_render_result_set(&self.ctx, &physical_plan).await?;
        assert!(build_res.main_pipeline.is_complete_pipeline()?);

        let max_threads = self.ctx.get_settings().get_max_threads()? as usize;
        build_res.set_max_threads(max_threads);

        let executor_settings = ExecutorSettings::try_create(self.ctx.clone())?;

        let mut pipelines = build_res.sources_pipelines;
        pipelines.push(build_res.main_pipeline);

        let complete_executor =
            PipelineCompleteExecutor::from_pipelines(pipelines, executor_settings)?;
        self.ctx.set_executor(complete_executor.get_inner())?;
        complete_executor.execute()?;
        // make sure the executor is dropped before the next loop.
        drop(complete_executor);
        // make sure the lock guard is dropped before the next loop.
        drop(lock_guard);

        Ok(false)
    }
}

pub fn build_recluster_physical_plan(
    tasks: ReclusterTasks,
    table_info: TableInfo,
    snapshot: Arc<TableSnapshot>,
    is_distributed: bool,
) -> Result<PhysicalPlan> {
    match tasks {
        ReclusterTasks::Recluster {
            tasks,
            remained_blocks,
            removed_segment_indexes,
            removed_segment_summary,
        } => {
            let mut root = PhysicalPlan::ReclusterSource(Box::new(ReclusterSource {
                tasks,
                table_info: table_info.clone(),
                plan_id: u32::MAX,
            }));

            if is_distributed {
                root = PhysicalPlan::Exchange(Exchange {
                    plan_id: 0,
                    input: Box::new(root),
                    kind: FragmentKind::Merge,
                    keys: vec![],
                    allow_adjust_parallelism: true,
                    ignore_exchange: false,
                });
            }
            let mut plan = PhysicalPlan::ReclusterSink(Box::new(ReclusterSink {
                input: Box::new(root),
                table_info,
                snapshot,
                remained_blocks,
                removed_segment_indexes,
                removed_segment_summary,
                plan_id: u32::MAX,
            }));
            plan.adjust_plan_id(&mut 0);
            Ok(plan)
        }
        ReclusterTasks::Compact(parts) => OptimizeTableInterpreter::build_physical_plan(
            parts,
            table_info,
            snapshot,
            is_distributed,
        ),
    }
}
