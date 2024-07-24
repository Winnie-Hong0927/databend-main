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

use std::collections::HashMap;
use std::sync::Arc;

use databend_common_base::runtime::profile::ProfileLabel;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::DataField;
use databend_common_expression::FunctionContext;
use databend_common_pipeline_core::processors::PlanScope;
use databend_common_pipeline_core::processors::PlanScopeGuard;
use databend_common_pipeline_core::Pipeline;
use databend_common_settings::Settings;
use databend_common_sql::binder::MergeIntoType;
use databend_common_sql::executor::PhysicalPlan;
use databend_common_sql::IndexType;

use super::PipelineBuilderData;
use crate::interpreters::CreateTableInterpreter;
use crate::pipelines::processors::transforms::HashJoinBuildState;
use crate::pipelines::processors::transforms::MaterializedCteState;
use crate::pipelines::processors::HashJoinState;
use crate::pipelines::PipelineBuildResult;
use crate::servers::flight::v1::exchange::DefaultExchangeInjector;
use crate::servers::flight::v1::exchange::ExchangeInjector;
use crate::sessions::QueryContext;

pub struct PipelineBuilder {
    pub(crate) ctx: Arc<QueryContext>,
    pub(crate) func_ctx: FunctionContext,
    pub(crate) main_pipeline: Pipeline,
    pub(crate) settings: Arc<Settings>,

    pub pipelines: Vec<Pipeline>,

    // probe data_fields for distributed merge into when source build
    pub merge_into_probe_data_fields: Option<Vec<DataField>>,
    pub join_state: Option<Arc<HashJoinBuildState>>,

    // Cte -> state, each cte has it's own state
    pub cte_state: HashMap<IndexType, Arc<MaterializedCteState>>,

    pub(crate) exchange_injector: Arc<dyn ExchangeInjector>,

    pub hash_join_states: HashMap<usize, Arc<HashJoinState>>,

    pub r_cte_scan_interpreters: Vec<CreateTableInterpreter>,
}

impl PipelineBuilder {
    pub fn create(
        func_ctx: FunctionContext,
        settings: Arc<Settings>,
        ctx: Arc<QueryContext>,
        scopes: Vec<PlanScope>,
    ) -> PipelineBuilder {
        PipelineBuilder {
            ctx,
            func_ctx,
            settings,
            pipelines: vec![],
            main_pipeline: Pipeline::with_scopes(scopes),
            exchange_injector: DefaultExchangeInjector::create(),
            cte_state: HashMap::new(),
            merge_into_probe_data_fields: None,
            join_state: None,
            hash_join_states: HashMap::new(),
            r_cte_scan_interpreters: vec![],
        }
    }

    pub fn finalize(mut self, plan: &PhysicalPlan) -> Result<PipelineBuildResult> {
        self.build_pipeline(plan)?;

        for source_pipeline in &self.pipelines {
            if !source_pipeline.is_complete_pipeline()? {
                return Err(ErrorCode::Internal(
                    "Source pipeline must be complete pipeline.",
                ));
            }
        }

        Ok(PipelineBuildResult {
            main_pipeline: self.main_pipeline,
            sources_pipelines: self.pipelines,
            exchange_injector: self.exchange_injector,
            builder_data: PipelineBuilderData {
                input_join_state: self.join_state,
                input_probe_schema: self.merge_into_probe_data_fields,
            },
            r_cte_scan_interpreters: self.r_cte_scan_interpreters,
        })
    }

    pub(crate) fn add_plan_scope(&mut self, plan: &PhysicalPlan) -> Result<Option<PlanScopeGuard>> {
        match plan {
            PhysicalPlan::EvalScalar(v) if v.exprs.is_empty() => Ok(None),
            PhysicalPlan::MergeInto(v) if v.merge_type != MergeIntoType::FullOperation => Ok(None),

            // hided plans in profile
            PhysicalPlan::Shuffle(_) => Ok(None),
            PhysicalPlan::ChunkCastSchema(_) => Ok(None),
            PhysicalPlan::ChunkFillAndReorder(_) => Ok(None),
            PhysicalPlan::ChunkMerge(_) => Ok(None),

            _ => {
                let desc = plan.get_desc()?;
                let plan_labels = plan.get_labels()?;
                let mut profile_labels = Vec::with_capacity(plan_labels.len());
                for (name, value) in plan_labels {
                    profile_labels.push(ProfileLabel::create(name, value));
                }

                let scope = PlanScope::create(
                    plan.get_id(),
                    plan.name(),
                    Arc::new(desc),
                    Arc::new(profile_labels),
                );
                Ok(Some(self.main_pipeline.add_plan_scope(scope)))
            }
        }
    }

    #[recursive::recursive]
    pub(crate) fn build_pipeline(&mut self, plan: &PhysicalPlan) -> Result<()> {
        let _guard = self.add_plan_scope(plan)?;
        match plan {
            PhysicalPlan::TableScan(scan) => self.build_table_scan(scan),
            PhysicalPlan::CteScan(scan) => self.build_cte_scan(scan),
            PhysicalPlan::ConstantTableScan(scan) => self.build_constant_table_scan(scan),
            PhysicalPlan::Filter(filter) => self.build_filter(filter),
            PhysicalPlan::EvalScalar(eval_scalar) => self.build_eval_scalar(eval_scalar),
            PhysicalPlan::AggregateExpand(aggregate) => self.build_aggregate_expand(aggregate),
            PhysicalPlan::AggregatePartial(aggregate) => self.build_aggregate_partial(aggregate),
            PhysicalPlan::AggregateFinal(aggregate) => self.build_aggregate_final(aggregate),
            PhysicalPlan::Window(window) => self.build_window(window),
            PhysicalPlan::Sort(sort) => self.build_sort(sort),
            PhysicalPlan::LocalShuffle(local_shuffle) => {
                self.build_local_shuffle_pipeline(local_shuffle)
            }
            PhysicalPlan::Limit(limit) => self.build_limit(limit),
            PhysicalPlan::RowFetch(row_fetch) => self.build_row_fetch(row_fetch),
            PhysicalPlan::HashJoin(join) => self.build_join(join),
            PhysicalPlan::ExchangeSink(sink) => self.build_exchange_sink(sink),
            PhysicalPlan::ExchangeSource(source) => self.build_exchange_source(source),
            PhysicalPlan::UnionAll(union_all) => self.build_union_all(union_all),
            PhysicalPlan::DistributedInsertSelect(insert_select) => {
                self.build_distributed_insert_select(insert_select)
            }
            PhysicalPlan::ProjectSet(project_set) => self.build_project_set(project_set),
            PhysicalPlan::Udf(udf) => self.build_udf(udf),
            PhysicalPlan::Exchange(_) => Err(ErrorCode::Internal(
                "Invalid physical plan with PhysicalPlan::Exchange",
            )),
            PhysicalPlan::RangeJoin(range_join) => self.build_range_join(range_join),
            PhysicalPlan::MaterializedCte(materialized_cte) => {
                self.build_materialized_cte(materialized_cte)
            }
            PhysicalPlan::CacheScan(cache_scan) => self.build_cache_scan(cache_scan),
            PhysicalPlan::ExpressionScan(expression_scan) => {
                self.build_expression_scan(expression_scan)
            }

            // Copy into.
            PhysicalPlan::CopyIntoTable(copy) => self.build_copy_into_table(copy),
            PhysicalPlan::CopyIntoLocation(copy) => self.build_copy_into_location(copy),

            // Delete.
            PhysicalPlan::DeleteSource(delete) => self.build_delete_source(delete),

            // Replace.
            PhysicalPlan::ReplaceAsyncSourcer(async_sourcer) => {
                self.build_async_sourcer(async_sourcer)
            }
            PhysicalPlan::ReplaceDeduplicate(deduplicate) => self.build_deduplicate(deduplicate),
            PhysicalPlan::ReplaceInto(replace) => self.build_replace_into(replace),

            // Merge into.
            PhysicalPlan::MergeInto(merge_into) => self.build_merge_into(merge_into),
            PhysicalPlan::MergeIntoSplit(merge_into_split) => {
                self.build_merge_into_split(merge_into_split)
            }
            PhysicalPlan::MergeIntoManipulate(merge_into_manipulate) => {
                self.build_merge_into_manipulate(merge_into_manipulate)
            }
            PhysicalPlan::MergeIntoOrganize(merge_into_organize) => {
                self.build_merge_into_organize(merge_into_organize)
            }

            // Commit.
            PhysicalPlan::CommitSink(plan) => self.build_commit_sink(plan),

            // Compact.
            PhysicalPlan::CompactSource(compact) => self.build_compact_source(compact),

            // Recluster.
            PhysicalPlan::ReclusterSource(recluster_source) => {
                self.build_recluster_source(recluster_source)
            }
            PhysicalPlan::ReclusterSink(recluster_sink) => {
                self.build_recluster_sink(recluster_sink)
            }

            // Update.
            PhysicalPlan::UpdateSource(update) => self.build_update_source(update),

            PhysicalPlan::Duplicate(duplicate) => self.build_duplicate(duplicate),
            PhysicalPlan::Shuffle(shuffle) => self.build_shuffle(shuffle),
            PhysicalPlan::ChunkFilter(chunk_filter) => self.build_chunk_filter(chunk_filter),
            PhysicalPlan::ChunkEvalScalar(chunk_project) => {
                self.build_chunk_eval_scalar(chunk_project)
            }
            PhysicalPlan::ChunkCastSchema(chunk_cast_schema) => {
                self.build_chunk_cast_schema(chunk_cast_schema)
            }
            PhysicalPlan::ChunkFillAndReorder(chunk_fill_and_reorder) => {
                self.build_chunk_fill_and_reorder(chunk_fill_and_reorder)
            }
            PhysicalPlan::ChunkAppendData(chunk_append_data) => {
                self.build_chunk_append_data(chunk_append_data)
            }
            PhysicalPlan::ChunkMerge(chunk_merge) => self.build_chunk_merge(chunk_merge),
            PhysicalPlan::ChunkCommitInsert(chunk_commit_insert) => {
                self.build_chunk_commit_insert(chunk_commit_insert)
            }
            PhysicalPlan::AsyncFunction(async_func) => self.build_async_function(async_func),
            PhysicalPlan::RecursiveCteScan(scan) => self.build_recursive_cte_scan(scan),
        }
    }
}
