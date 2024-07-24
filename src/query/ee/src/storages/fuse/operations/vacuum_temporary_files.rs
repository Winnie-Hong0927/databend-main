// Copyright 2023 Databend Cloud
//
// Licensed under the Elastic License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.elastic.co/licensing/elastic-license
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use databend_common_exception::Result;
use databend_common_storage::DataOperator;
use futures_util::stream;
use futures_util::TryStreamExt;
use log::info;
use opendal::Entry;
use opendal::EntryMode;
use opendal::Metakey;

// Default retention duration for temporary files: 3 days.
const DEFAULT_RETAIN_DURATION: Duration = Duration::from_secs(60 * 60 * 24 * 3);

#[async_backtrace::framed]
pub async fn do_vacuum_temporary_files(
    temporary_dir: String,
    retain: Option<Duration>,
    limit: usize,
) -> Result<usize> {
    if limit == 0 {
        return Ok(0);
    }

    let expire_time = retain.unwrap_or(DEFAULT_RETAIN_DURATION).as_millis() as i64;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let operator = DataOperator::instance().operator();

    let temporary_dir = format!("{}/", temporary_dir);

    let mut ds = operator
        .lister_with(&temporary_dir)
        .metakey(Metakey::Mode | Metakey::LastModified)
        .await?;

    let mut removed_temp_files = 0;
    let mut total_cleaned_size = 0;
    let mut total_batch_size = 0;
    let start_time = Instant::now();

    while removed_temp_files < limit {
        let instant = Instant::now();
        let mut end_of_stream = true;
        let mut remove_temp_files_path = Vec::with_capacity(1000);
        let mut batch_size = 0;

        while let Some(de) = ds.try_next().await? {
            let meta = de.metadata();

            match meta.mode() {
                EntryMode::DIR => {
                    let life_mills =
                        match operator.is_exist(&format!("{}finished", de.path())).await? {
                            true => 0,
                            false => expire_time,
                        };

                    vacuum_finished_query(
                        start_time,
                        &mut removed_temp_files,
                        &mut total_cleaned_size,
                        &mut batch_size,
                        &de,
                        limit,
                        timestamp,
                        life_mills,
                    )
                    .await?;

                    if removed_temp_files >= limit {
                        end_of_stream = false;
                        break;
                    }
                }
                EntryMode::FILE => {
                    if let Some(modified) = meta.last_modified() {
                        if timestamp - modified.timestamp_millis() >= expire_time {
                            removed_temp_files += 1;
                            remove_temp_files_path.push(de.path().to_string());
                            batch_size += meta.content_length() as usize;

                            if removed_temp_files >= limit || remove_temp_files_path.len() >= 1000 {
                                end_of_stream = false;
                                break;
                            }
                        }
                    }
                }
                EntryMode::Unknown => unreachable!(),
            }
        }

        if !remove_temp_files_path.is_empty() {
            let cur_removed = remove_temp_files_path.len();
            total_cleaned_size += batch_size;
            operator
                .remove_via(stream::iter(remove_temp_files_path))
                .await?;

            // Log for the current batch
            info!(
                "vacuum removed {} temp files in {:?}(elapsed: {} seconds), batch size: {} bytes",
                cur_removed,
                temporary_dir,
                instant.elapsed().as_secs(),
                batch_size
            );

            // Log for the total progress
            info!(
                "Total progress: {} files removed, total cleaned size: {} bytes, total batch size: {} bytes",
                removed_temp_files,
                total_cleaned_size,
                total_batch_size + batch_size
            );
        }

        total_batch_size += batch_size;

        if end_of_stream {
            break;
        }
    }

    // Log for the final total progress
    info!(
        "vacuum finished, total cleaned {} files, total cleaned size: {} bytes, total elapsed: {} seconds",
        removed_temp_files,
        total_cleaned_size,
        start_time.elapsed().as_secs()
    );

    Ok(removed_temp_files)
}

async fn vacuum_finished_query(
    total_instant: Instant,
    removed_temp_files: &mut usize,
    total_cleaned_size: &mut usize,
    batch_size: &mut usize,
    de: &Entry,
    limit: usize,
    timestamp: i64,
    life_mills: i64,
) -> Result<()> {
    let operator = DataOperator::instance().operator();

    let mut all_files_removed = true;
    let mut ds = operator
        .lister_with(de.path())
        .metakey(Metakey::Mode | Metakey::LastModified)
        .await?;

    while *removed_temp_files < limit {
        let instant = Instant::now();

        let mut end_of_stream = true;
        let mut all_each_files_removed = true;
        let mut remove_temp_files_path = Vec::with_capacity(1001);

        while let Some(de) = ds.try_next().await? {
            let meta = de.metadata();
            if meta.is_file() {
                if de.name() == "finished" {
                    continue;
                }

                if let Some(modified) = meta.last_modified() {
                    if timestamp - modified.timestamp_millis() >= life_mills {
                        *removed_temp_files += 1;
                        remove_temp_files_path.push(de.path().to_string());
                        *batch_size += meta.content_length() as usize;

                        if *removed_temp_files >= limit || remove_temp_files_path.len() >= 1000 {
                            end_of_stream = false;
                            break;
                        }

                        continue;
                    }
                }
            }

            all_each_files_removed = false;
        }

        all_files_removed &= all_each_files_removed;

        if !remove_temp_files_path.is_empty() {
            let cur_removed = remove_temp_files_path.len();
            *total_cleaned_size += *batch_size;
            operator
                .remove_via(stream::iter(remove_temp_files_path))
                .await?;

            // Log for the current batch
            info!(
                "vacuum removed {} temp files in {:?}(elapsed: {} seconds), batch size: {} bytes",
                cur_removed,
                de.path(),
                instant.elapsed().as_secs(),
                *batch_size
            );

            // Log for the total progress
            info!(
                "Total progress: {} files removed, total cleaned size: {} bytes, total elapsed: {} seconds",
                *removed_temp_files,
                *total_cleaned_size,
                total_instant.elapsed().as_secs()
            );
        }

        if end_of_stream {
            break;
        }
    }

    if all_files_removed {
        operator.delete(&format!("{}finished", de.path())).await?;
        operator.delete(de.path()).await?;
    }

    Ok(())
}
