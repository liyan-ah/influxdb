//! Implementation of Deduplication algorithm

use std::{cmp::Ordering, ops::Range, sync::Arc};

use arrow::{
    array::{ArrayRef, UInt64Array},
    compute::TakeOptions,
    error::Result as ArrowResult,
    record_batch::RecordBatch,
};

use datafusion::physical_plan::{
    coalesce_batches::concat_batches, expressions::PhysicalSortExpr, PhysicalExpr, SQLMetric,
};
use observability_deps::tracing::trace;

// Handles the deduplication across potentially multiple
// [`RecordBatch`]es which are already sorted on a primary key,
// including primary keys which straddle RecordBatch boundaries
#[derive(Debug)]
pub(crate) struct RecordBatchDeduplicator {
    sort_keys: Vec<PhysicalSortExpr>,
    last_batch: Option<RecordBatch>,
    num_dupes: Arc<SQLMetric>,
}

#[derive(Debug)]
struct DuplicateRanges {
    ///  `is_sort_key[col_idx] = true` if the the input column at
    ///  `col_idx` is present in sort keys
    is_sort_key: Vec<bool>,

    /// ranges of row indices where the sort key columns have the
    /// same values
    ranges: Vec<Range<usize>>,
}

impl RecordBatchDeduplicator {
    pub fn new(sort_keys: Vec<PhysicalSortExpr>, num_dupes: Arc<SQLMetric>, last_batch: Option<RecordBatch>) -> Self {
        Self {
            sort_keys,
            last_batch,
            num_dupes,
        }
    }

    /// Push a new RecordBatch into the indexer. Returns a
    /// deduplicated RecordBatch and remembers any currently opened
    /// groups
    pub fn push(&mut self, batch: RecordBatch) -> ArrowResult<RecordBatch> {
        // If we had a previous batch of rows, add it in here
        //
        // Potential optimization would be to check if the sort key is actually the same
        // for the first row in the new batch and skip this concat if that is the case
        let batch = if let Some(last_batch) = self.last_batch.take() {
            let schema = last_batch.schema();
            let row_count = last_batch.num_rows() + batch.num_rows();
            concat_batches(&schema, &[last_batch, batch], row_count)?
        } else {
            batch
        };

        let mut dupe_ranges = self.compute_ranges(&batch)?;

        // The last partition may span batches so we can't emit it
        // until we have seen the next batch (or we are at end of
        // stream)
        let last_range = dupe_ranges.ranges.pop();

        let output_record_batch = self.output_from_ranges(&batch, &dupe_ranges)?;

        // Now, save the last bit of the pk
        if let Some(last_range) = last_range {
            let len = last_range.end - last_range.start;
            let last_batch = Self::slice_record_batch(&batch, last_range.start, len)?;
            self.last_batch = Some(last_batch);
        }

        Ok(output_record_batch)
    }

    /// Return last_batch if it does not overlap with the given batch
    /// Note that since last_batch, if exists, will include at least one row and all of its rows will have the same key
    pub fn last_batch_with_no_same_sort_key(&mut self, batch: &RecordBatch) -> Option<RecordBatch> {
        // Take the previous batch, if any, out of it storage self.last_batch
        if let Some(last_batch) = self.last_batch.take() {
            let schema = last_batch.schema();
            // Build sorted columns for last_batch and current one
            let last_batch_key_columns = self
                .sort_keys
                .iter()
                .map(|skey| {
                    // figure out the index of the key columns
                    let name = get_col_name(skey.expr.as_ref());
                    let index = schema.index_of(name).unwrap();

                    // Key column of last_batch of this index
                    let last_batch_array = last_batch.column(index);
                    if last_batch_array.len() == 0 {
                        panic!("Key column, {}, of last_batch has no data", name);
                    }
                    arrow::compute::SortColumn {
                        values: Arc::clone(last_batch_array),
                        options: Some(skey.options),
                    }
                })
                .collect::<Vec<arrow::compute::SortColumn>>();

            // Build sorted columns for current batch
            let batch_key_columns = self
                .sort_keys
                .iter()
                .map(|skey| {
                    // figure out the index of the key columns
                    let name = get_col_name(skey.expr.as_ref());
                    let index = schema.index_of(name).unwrap();

                    // Key column of current batch of this index
                    let array = batch.column(index);
                    if array.len() == 0 {
                        panic!("Key column, {}, of current batch has no data", name);
                    }
                    arrow::compute::SortColumn {
                        values: Arc::clone(array),
                        options: Some(skey.options),
                    }
                })
                .collect::<Vec<arrow::compute::SortColumn>>();

            // Zip the 2 key set of columns for comparison
            let zipped = last_batch_key_columns.iter().zip(batch_key_columns.iter());

            // Compare sort keys of the first row of the given batch the the last_batch
            // Note that the batches are sorted and all rows of last_batch have the same sort keys so
            // only need to compare the first row
            let mut same = true;
            for (l, r) in zipped {
                match (l.values.is_valid(0), r.values.is_valid(0)) {
                    (true, true) => {
                        // Now the actual comparison
                        let c = arrow::array::build_compare(l.values.as_ref(), r.values.as_ref())
                            .unwrap();

                        match c(0, 0) {
                            // again only compare the first row
                            Ordering::Equal => {}
                            _ => {
                                same = false;
                                break;
                            }
                        }
                    }
                    _ => {
                        // The values of this column pair are not the same, no need to compare further
                        same = false;
                        break;
                    }
                }
            }

            if same {
                // The batches overlap and need to be concatinated
                // So, store it back in self.last_batch for the concat_batches later
                self.last_batch = Some(last_batch);
                None
            } else {
                // The batches do not overlap, return the last_batch to be sent downstream and reset it here
                self.last_batch = None;
                Some(last_batch)
            }
        } else {
            None
        }
    }

    /// Consume the indexer, returning any remaining record batches for output
    pub fn finish(mut self) -> ArrowResult<Option<RecordBatch>> {
        self.last_batch
            .take()
            .map(|last_batch| {
                let dupe_ranges = self.compute_ranges(&last_batch)?;
                self.output_from_ranges(&last_batch, &dupe_ranges)
            })
            .transpose()
    }

    /// Computes the ranges where the sort key has the same values
    fn compute_ranges(&self, batch: &RecordBatch) -> ArrowResult<DuplicateRanges> {
        let schema = batch.schema();
        // is_sort_key[col_idx] = true if it is present in sort keys
        let mut is_sort_key: Vec<bool> = vec![false; batch.columns().len()];

        // Figure out where the partitions are:
        let columns: Vec<_> = self
            .sort_keys
            .iter()
            .map(|skey| {
                // figure out what input column this is for
                let name = get_col_name(skey.expr.as_ref());
                let index = schema.index_of(name).unwrap();

                is_sort_key[index] = true;

                let array = batch.column(index);

                arrow::compute::SortColumn {
                    values: Arc::clone(array),
                    options: Some(skey.options),
                }
            })
            .collect();

        // Compute partitions (aka breakpoints between the ranges)
        let ranges = arrow::compute::lexicographical_partition_ranges(&columns)?;

        Ok(DuplicateRanges {
            is_sort_key,
            ranges,
        })
    }

    /// Compute the output record batch that includes the specified ranges
    fn output_from_ranges(
        &self,
        batch: &RecordBatch,
        dupe_ranges: &DuplicateRanges,
    ) -> ArrowResult<RecordBatch> {
        let ranges = &dupe_ranges.ranges;

        // each range is at least 1 large, so any that have more than
        // 1 are duplicates
        let num_dupes = ranges.iter().map(|r| r.end - r.start - 1).sum();

        self.num_dupes.add(num_dupes);

        // Special case when no ranges are duplicated (so just emit input as output)
        if num_dupes == 0 {
            trace!(num_rows = batch.num_rows(), "No dupes");
            Self::slice_record_batch(&batch, 0, ranges.len())
        } else {
            trace!(num_dupes, num_rows = batch.num_rows(), "dupes");

            // Use take kernel
            let sort_key_indices = self.compute_sort_key_indices(&ranges);

            let take_options = Some(TakeOptions {
                check_bounds: false,
            });

            // Form each new column by `take`ing the indices as needed
            let new_columns = batch
                .columns()
                .iter()
                .enumerate()
                .map(|(input_index, input_array)| {
                    if dupe_ranges.is_sort_key[input_index] {
                        arrow::compute::take(
                            input_array.as_ref(),
                            &sort_key_indices,
                            take_options.clone(),
                        )
                    } else {
                        // pick the last non null value
                        let field_indices = self.compute_field_indices(&ranges, input_array);

                        arrow::compute::take(
                            input_array.as_ref(),
                            &field_indices,
                            take_options.clone(),
                        )
                    }
                })
                .collect::<ArrowResult<Vec<ArrayRef>>>()?;
            RecordBatch::try_new(batch.schema(), new_columns)
        }
    }

    /// Returns an array of indices, one for each input range (which
    /// index is arbitrary as all the values are the same for the sort
    /// column in each pk group)
    ///
    /// ranges: 0-1, 2-4, 5-6 --> Array[0, 2, 5]
    fn compute_sort_key_indices(&self, ranges: &[Range<usize>]) -> UInt64Array {
        ranges.iter().map(|r| Some(r.start as u64)).collect()
    }

    /// Returns an array of indices, one for each input range that
    /// return the first non-null value of `input_array` in that range
    /// (aka it will pick the index of the field value to use for each
    /// pk group)
    ///
    /// ranges: 0-1, 2-4, 5-6
    /// input array: A, NULL, NULL, C, NULL, NULL
    /// --> Array[0, 3, 5]
    fn compute_field_indices(
        &self,
        ranges: &[Range<usize>],
        input_array: &ArrayRef,
    ) -> UInt64Array {
        ranges
            .iter()
            .map(|r| {
                let value_index = r
                    .clone()
                    .filter(|&i| input_array.is_valid(i))
                    .last()
                    .map(|i| i as u64)
                    // if all field values are none, pick one arbitrarily
                    .unwrap_or(r.start as u64);
                Some(value_index)
            })
            .collect()
    }

    /// Create a new record batch from offset --> len
    ///
    /// https://github.com/apache/arrow-rs/issues/460 for adding this upstream
    fn slice_record_batch(
        batch: &RecordBatch,
        offset: usize,
        len: usize,
    ) -> ArrowResult<RecordBatch> {
        let schema = batch.schema();
        let new_columns: Vec<_> = batch
            .columns()
            .iter()
            .map(|old_column| old_column.slice(offset, len))
            .collect();

        RecordBatch::try_new(schema, new_columns)
    }
}

/// Get column name out of the `expr`. TODO use
/// internal_types::schema::SortKey instead.
fn get_col_name(expr: &dyn PhysicalExpr) -> &str {
    expr.as_any()
        .downcast_ref::<datafusion::physical_plan::expressions::Column>()
        .expect("expected column reference")
        .name()
}

#[cfg(test)]
mod test {
    use arrow::compute::SortOptions;
    use arrow::{
        array::{ArrayRef, Float64Array, StringArray},
        record_batch::RecordBatch,
    };

    use arrow_util::assert_batches_eq;
    use datafusion::physical_plan::expressions::col;
    use datafusion::physical_plan::MetricType;

    use super::*;

    #[tokio::test]
    async fn test_non_overlapped_sorted_batches_one_key_column() {
        // Sorted key: t1

        // Last batch
        // t1 | t2 | f1 | f2
        // ---+----+----+----
        //  a | b  | 1  | 2
        //  a | c  | 3  |
        //  a | c  | 4  | 5

        // Current batch
        //  ====(next batch)====
        //  b | c  |    | 6
        //  b | d  | 7  | 8

        // Non overlapped => return last batch
        // Expected output = Last batch
        // t1 | t2 | f1 | f2
        // ---+----+----+----
        //  a | b  | 1  | 2
        //  a | c  | 3  |
        //  a | c  | 4  | 5

        // Columns of last_batch
        let t1 = StringArray::from(vec![Some("a"), Some("a"), Some("a")]);
        let t2 = StringArray::from(vec![Some("b"), Some("c"), Some("c")]);
        let f1 = Float64Array::from(vec![Some(1.0), Some(3.0), Some(4.0)]);
        let f2 = Float64Array::from(vec![Some(2.0), None, Some(5.0)]);

        let last_batch = RecordBatch::try_from_iter(vec![
            ("t1", Arc::new(t1) as ArrayRef),
            ("t2", Arc::new(t2) as ArrayRef),
            ("f1", Arc::new(f1) as ArrayRef),
            ("f2", Arc::new(f2) as ArrayRef),
        ])
        .unwrap();

        // Columns of current_batch
        let t1 = StringArray::from(vec![Some("b"), Some("b")]);
        let t2 = StringArray::from(vec![Some("c"), Some("d")]);
        let f1 = Float64Array::from(vec![None, Some(7.0)]);
        let f2 = Float64Array::from(vec![Some(6.0), Some(8.0)]);

        let current_batch = RecordBatch::try_from_iter(vec![
            ("t1", Arc::new(t1) as ArrayRef),
            ("t2", Arc::new(t2) as ArrayRef),
            ("f1", Arc::new(f1) as ArrayRef),
            ("f2", Arc::new(f2) as ArrayRef),
        ])
        .unwrap();

        let sort_keys = vec![PhysicalSortExpr {
            expr: col("t1"),
            options: SortOptions {
                descending: false,
                nulls_first: false,
            },
        }];

        let num_dupes = Arc::new(SQLMetric::new(MetricType::Counter));
        let mut dedupe =
            RecordBatchDeduplicator::new(sort_keys, num_dupes, Some(last_batch));

        let results = dedupe
            .last_batch_with_no_same_sort_key(&current_batch)
            .unwrap();

        let expected = vec![
            "+----+----+----+----+",
            "| t1 | t2 | f1 | f2 |",
            "+----+----+----+----+",
            "| a  | b  | 1  | 2  |",
            "| a  | c  | 3  |    |",
            "| a  | c  | 4  | 5  |",
            "+----+----+----+----+",
        ];
        assert_batches_eq!(&expected, &[results]);
    }

    #[tokio::test]
    async fn test_non_overlapped_sorted_batches_two_key_columns() {
        // Sorted key: t1, t2

        // Last batch
        // t1 | t2 | f1 | f2
        // ---+----+----+----
        //  a | b  | 1  | 2
        //  a | c  | 3  |
        //  a | c  | 4  | 5

        // Current batch
        //  ====(next batch)====
        //  b | c  |    | 6
        //  b | d  | 7  | 8

        // Non overlapped => return last batch
        // Expected output = Last batch
        // t1 | t2 | f1 | f2
        // ---+----+----+----
        //  a | b  | 1  | 2
        //  a | c  | 3  |
        //  a | c  | 4  | 5

        // Columns of last_batch
        let t1 = StringArray::from(vec![Some("a"), Some("a"), Some("a")]);
        let t2 = StringArray::from(vec![Some("b"), Some("c"), Some("c")]);
        let f1 = Float64Array::from(vec![Some(1.0), Some(3.0), Some(4.0)]);
        let f2 = Float64Array::from(vec![Some(2.0), None, Some(5.0)]);

        let last_batch = RecordBatch::try_from_iter(vec![
            ("t1", Arc::new(t1) as ArrayRef),
            ("t2", Arc::new(t2) as ArrayRef),
            ("f1", Arc::new(f1) as ArrayRef),
            ("f2", Arc::new(f2) as ArrayRef),
        ])
        .unwrap();

        // Columns of current_batch
        let t1 = StringArray::from(vec![Some("b"), Some("b")]);
        let t2 = StringArray::from(vec![Some("c"), Some("d")]);
        let f1 = Float64Array::from(vec![None, Some(7.0)]);
        let f2 = Float64Array::from(vec![Some(6.0), Some(8.0)]);

        let current_batch = RecordBatch::try_from_iter(vec![
            ("t1", Arc::new(t1) as ArrayRef),
            ("t2", Arc::new(t2) as ArrayRef),
            ("f1", Arc::new(f1) as ArrayRef),
            ("f2", Arc::new(f2) as ArrayRef),
        ])
        .unwrap();

        let sort_keys = vec![
            PhysicalSortExpr {
                expr: col("t1"),
                options: SortOptions {
                    descending: false,
                    nulls_first: false,
                },
            },
            PhysicalSortExpr {
                expr: col("t2"),
                options: SortOptions {
                    descending: false,
                    nulls_first: false,
                },
            },
        ];

        let num_dupes = Arc::new(SQLMetric::new(MetricType::Counter));
        let mut dedupe =
            RecordBatchDeduplicator::new(sort_keys, num_dupes, Some(last_batch));

        let results = dedupe
            .last_batch_with_no_same_sort_key(&current_batch)
            .unwrap();

        let expected = vec![
            "+----+----+----+----+",
            "| t1 | t2 | f1 | f2 |",
            "+----+----+----+----+",
            "| a  | b  | 1  | 2  |",
            "| a  | c  | 3  |    |",
            "| a  | c  | 4  | 5  |",
            "+----+----+----+----+",
        ];
        assert_batches_eq!(&expected, &[results]);
    }

    #[tokio::test]
    async fn test_overlapped_sorted_batches_one_key_column() {
        // Sorted key: t1

        // Last batch
        // t1 | t2 | f1 | f2
        // ---+----+----+----
        //  a | b  | 1  | 2
        //  a | b  | 3  |

        // Current batch
        //  ====(next batch)====
        //  a | b  |    | 6
        //  b | d  | 7  | 8

        // Overlapped => return None

        // Columns of last_batch
        let t1 = StringArray::from(vec![Some("a"), Some("a")]);
        let t2 = StringArray::from(vec![Some("b"), Some("b")]);
        let f1 = Float64Array::from(vec![Some(1.0), Some(3.0)]);
        let f2 = Float64Array::from(vec![Some(2.0), None]);

        let last_batch = RecordBatch::try_from_iter(vec![
            ("t1", Arc::new(t1) as ArrayRef),
            ("t2", Arc::new(t2) as ArrayRef),
            ("f1", Arc::new(f1) as ArrayRef),
            ("f2", Arc::new(f2) as ArrayRef),
        ])
        .unwrap();

        // Columns of current_batch
        let t1 = StringArray::from(vec![Some("a"), Some("b")]);
        let t2 = StringArray::from(vec![Some("b"), Some("d")]);
        let f1 = Float64Array::from(vec![None, Some(7.0)]);
        let f2 = Float64Array::from(vec![Some(6.0), Some(8.0)]);

        let current_batch = RecordBatch::try_from_iter(vec![
            ("t1", Arc::new(t1) as ArrayRef),
            ("t2", Arc::new(t2) as ArrayRef),
            ("f1", Arc::new(f1) as ArrayRef),
            ("f2", Arc::new(f2) as ArrayRef),
        ])
        .unwrap();

        let sort_keys = vec![PhysicalSortExpr {
            expr: col("t1"),
            options: SortOptions {
                descending: false,
                nulls_first: false,
            },
        }];

        let num_dupes = Arc::new(SQLMetric::new(MetricType::Counter));
        let mut dedupe =
            RecordBatchDeduplicator::new(sort_keys, num_dupes, Some(last_batch));

        let results = dedupe.last_batch_with_no_same_sort_key(&current_batch);
        assert!(results.is_none());
    }

    #[tokio::test]
    async fn test_overlapped_sorted_batches_two_key_columns() {
        // Sorted key: t1, t2

        // Last batch
        // t1 | t2 | f1 | f2
        // ---+----+----+----
        //  a | b  | 1  | 2
        //  a | b  | 3  |

        // Current batch
        //  ====(next batch)====
        //  a | b  |    | 6
        //  b | d  | 7  | 8

        // Overlapped => return None

        // Columns of last_batch
        let t1 = StringArray::from(vec![Some("a"), Some("a")]);
        let t2 = StringArray::from(vec![Some("b"), Some("b")]);
        let f1 = Float64Array::from(vec![Some(1.0), Some(3.0)]);
        let f2 = Float64Array::from(vec![Some(2.0), None]);

        let last_batch = RecordBatch::try_from_iter(vec![
            ("t1", Arc::new(t1) as ArrayRef),
            ("t2", Arc::new(t2) as ArrayRef),
            ("f1", Arc::new(f1) as ArrayRef),
            ("f2", Arc::new(f2) as ArrayRef),
        ])
        .unwrap();

        // Columns of current_batch
        let t1 = StringArray::from(vec![Some("a"), Some("b")]);
        let t2 = StringArray::from(vec![Some("b"), Some("d")]);
        let f1 = Float64Array::from(vec![None, Some(7.0)]);
        let f2 = Float64Array::from(vec![Some(6.0), Some(8.0)]);

        let current_batch = RecordBatch::try_from_iter(vec![
            ("t1", Arc::new(t1) as ArrayRef),
            ("t2", Arc::new(t2) as ArrayRef),
            ("f1", Arc::new(f1) as ArrayRef),
            ("f2", Arc::new(f2) as ArrayRef),
        ])
        .unwrap();

        let sort_keys = vec![
            PhysicalSortExpr {
                expr: col("t1"),
                options: SortOptions {
                    descending: false,
                    nulls_first: false,
                },
            },
            PhysicalSortExpr {
                expr: col("t2"),
                options: SortOptions {
                    descending: false,
                    nulls_first: false,
                },
            },
        ];

        let num_dupes = Arc::new(SQLMetric::new(MetricType::Counter));
        let mut dedupe =
            RecordBatchDeduplicator::new(sort_keys, num_dupes, Some(last_batch));

        let results = dedupe.last_batch_with_no_same_sort_key(&current_batch);
        assert!(results.is_none());
    }
}
