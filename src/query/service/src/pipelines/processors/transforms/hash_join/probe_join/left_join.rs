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

use std::iter::TrustedLen;
use std::sync::atomic::Ordering;

use common_arrow::arrow::bitmap::Bitmap;
use common_arrow::arrow::bitmap::MutableBitmap;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::BlockEntry;
use common_expression::DataBlock;
use common_expression::Scalar;
use common_expression::Value;
use common_hashtable::HashJoinHashtableLike;

use crate::pipelines::processors::transforms::hash_join::ProbeState;
use crate::pipelines::processors::JoinHashTable;
use crate::sql::plans::JoinType;

impl JoinHashTable {
    pub(crate) fn probe_left_join<'a, H: HashJoinHashtableLike, IT>(
        &self,
        hash_table: &H,
        probe_state: &mut ProbeState,
        keys_iter: IT,
        input: &DataBlock,
        is_probe_projected: bool,
    ) -> Result<Vec<DataBlock>>
    where
        IT: Iterator<Item = &'a H::Key> + TrustedLen,
        H::Key: 'a,
    {
        let input_num_rows = input.num_rows();
        let max_block_size = probe_state.max_block_size;
        let valids = &probe_state.valids;
        let true_validity = &probe_state.true_validity;
        let probe_indexes = &mut probe_state.probe_indexes;
        let local_build_indexes = &mut probe_state.build_indexes;
        let local_build_indexes_ptr = local_build_indexes.as_mut_ptr();
        // Safe to unwrap.
        let probe_unmatched_indexes = probe_state.probe_unmatched_indexes.as_mut().unwrap();

        let mut matched_num = 0;
        let mut probe_indexes_occupied = 0;
        let mut probe_unmatched_indexes_occupied = 0;
        let mut result_blocks = vec![];

        let data_blocks = self.row_space.chunks.read();
        let data_blocks = data_blocks
            .iter()
            .map(|c| &c.data_block)
            .collect::<Vec<_>>();
        let build_num_rows = data_blocks
            .iter()
            .fold(0, |acc, chunk| acc + chunk.num_rows());
        let is_build_projected = self.is_build_projected.load(Ordering::Relaxed);
        let outer_scan_map = unsafe { &mut *self.outer_scan_map.get() };

        // Start to probe hash table.
        for (i, key) in keys_iter.enumerate() {
            let (mut probe_matched, mut incomplete_ptr) =
                if self.hash_join_desc.from_correlated_subquery {
                    hash_table.probe_hash_table(
                        key,
                        local_build_indexes_ptr,
                        matched_num,
                        max_block_size,
                    )
                } else {
                    self.probe_key(
                        hash_table,
                        key,
                        valids,
                        i,
                        local_build_indexes_ptr,
                        matched_num,
                        max_block_size,
                    )
                };
            let mut total_probe_matched = 0;
            if probe_matched > 0 {
                total_probe_matched += probe_matched;
                if self.hash_join_desc.join_type == JoinType::LeftSingle && total_probe_matched > 1
                {
                    return Err(ErrorCode::Internal(
                        "Scalar subquery can't return more than one row",
                    ));
                }
                matched_num += probe_matched;
                probe_indexes[probe_indexes_occupied] = (i as u32, probe_matched as u32);
                probe_indexes_occupied += 1;
            } else {
                probe_unmatched_indexes[probe_unmatched_indexes_occupied] = (i as u32, 1);
                probe_unmatched_indexes_occupied += 1;
                if probe_unmatched_indexes_occupied >= max_block_size {
                    if self.interrupt.load(Ordering::Relaxed) {
                        return Err(ErrorCode::AbortedQuery(
                            "Aborted query, because the server is shutting down or the query was killed.",
                        ));
                    }
                    result_blocks.push(self.create_left_join_null_block(
                        input,
                        probe_unmatched_indexes,
                        probe_unmatched_indexes_occupied,
                        is_probe_projected,
                        is_build_projected,
                    )?);
                    probe_unmatched_indexes_occupied = 0;
                }
            }
            if matched_num >= max_block_size || i == input_num_rows - 1 {
                loop {
                    if self.interrupt.load(Ordering::Relaxed) {
                        return Err(ErrorCode::AbortedQuery(
                            "Aborted query, because the server is shutting down or the query was killed.",
                        ));
                    }

                    let probe_block = if is_probe_projected {
                        let mut probe_block = DataBlock::take_compacted_indices(
                            input,
                            &probe_indexes[0..probe_indexes_occupied],
                            matched_num,
                        )?;
                        // For full join, wrap nullable for probe block
                        if self.hash_join_desc.join_type == JoinType::Full {
                            let nullable_probe_columns = if matched_num == max_block_size {
                                probe_block
                                    .columns()
                                    .iter()
                                    .map(|c| Self::set_validity(c, max_block_size, true_validity))
                                    .collect::<Vec<_>>()
                            } else {
                                let mut validity = MutableBitmap::new();
                                validity.extend_constant(matched_num, true);
                                let validity: Bitmap = validity.into();
                                probe_block
                                    .columns()
                                    .iter()
                                    .map(|c| Self::set_validity(c, matched_num, &validity))
                                    .collect::<Vec<_>>()
                            };
                            probe_block = DataBlock::new(nullable_probe_columns, matched_num);
                        }
                        Some(probe_block)
                    } else {
                        None
                    };
                    let build_block = if is_build_projected {
                        let build_block = self.row_space.gather(
                            &local_build_indexes[0..matched_num],
                            &data_blocks,
                            &build_num_rows,
                        )?;
                        // For left join, wrap nullable for build block
                        let (nullable_columns, num_rows) = if build_num_rows == 0 {
                            (
                                build_block
                                    .columns()
                                    .iter()
                                    .map(|c| BlockEntry {
                                        value: Value::Scalar(Scalar::Null),
                                        data_type: c.data_type.wrap_nullable(),
                                    })
                                    .collect::<Vec<_>>(),
                                matched_num,
                            )
                        } else if matched_num == max_block_size {
                            (
                                build_block
                                    .columns()
                                    .iter()
                                    .map(|c| Self::set_validity(c, max_block_size, true_validity))
                                    .collect::<Vec<_>>(),
                                max_block_size,
                            )
                        } else {
                            let mut validity = MutableBitmap::new();
                            validity.extend_constant(matched_num, true);
                            let validity: Bitmap = validity.into();
                            (
                                build_block
                                    .columns()
                                    .iter()
                                    .map(|c| Self::set_validity(c, matched_num, &validity))
                                    .collect::<Vec<_>>(),
                                matched_num,
                            )
                        };
                        Some(DataBlock::new(nullable_columns, num_rows))
                    } else {
                        None
                    };
                    let result_block = self.merge_eq_block(probe_block, build_block, matched_num);

                    if !result_block.is_empty() {
                        result_blocks.push(result_block);
                        if self.hash_join_desc.join_type == JoinType::Full {
                            for row_ptr in local_build_indexes.iter().take(matched_num) {
                                outer_scan_map[row_ptr.chunk_index as usize]
                                    [row_ptr.row_index as usize] = true;
                            }
                        }
                    }

                    matched_num = 0;
                    probe_indexes_occupied = 0;

                    if incomplete_ptr == 0 {
                        break;
                    }
                    (probe_matched, incomplete_ptr) = hash_table.next_incomplete_ptr(
                        key,
                        incomplete_ptr,
                        local_build_indexes_ptr,
                        matched_num,
                        max_block_size,
                    );

                    if probe_matched > 0 {
                        total_probe_matched += probe_matched;
                        if self.hash_join_desc.join_type == JoinType::LeftSingle
                            && total_probe_matched > 1
                        {
                            return Err(ErrorCode::Internal(
                                "Scalar subquery can't return more than one row",
                            ));
                        }
                        matched_num += probe_matched;
                        probe_indexes[probe_indexes_occupied] = (i as u32, probe_matched as u32);
                        probe_indexes_occupied += 1;
                    }

                    if matched_num < max_block_size && i != input_num_rows - 1 {
                        break;
                    }
                }
            }
        }

        if probe_unmatched_indexes_occupied == 0 {
            return Ok(result_blocks);
        }
        result_blocks.push(self.create_left_join_null_block(
            input,
            probe_unmatched_indexes,
            probe_unmatched_indexes_occupied,
            is_probe_projected,
            is_build_projected,
        )?);
        Ok(result_blocks)
    }

    pub(crate) fn probe_left_join_with_conjunct<'a, H: HashJoinHashtableLike, IT>(
        &self,
        hash_table: &H,
        probe_state: &mut ProbeState,
        keys_iter: IT,
        input: &DataBlock,
        is_probe_projected: bool,
    ) -> Result<Vec<DataBlock>>
    where
        IT: Iterator<Item = &'a H::Key> + TrustedLen,
        H::Key: 'a,
    {
        let input_num_rows = input.num_rows();
        let max_block_size = probe_state.max_block_size;
        let valids = &probe_state.valids;
        let true_validity = &probe_state.true_validity;
        let probe_indexes = &mut probe_state.probe_indexes;
        let local_build_indexes = &mut probe_state.build_indexes;
        let local_build_indexes_ptr = local_build_indexes.as_mut_ptr();
        if input_num_rows > probe_state.row_state.as_ref().unwrap().len() {
            probe_state.row_state = Some(vec![0; input_num_rows]);
        }
        // The row_state is used to record whether a row in probe input is matched.
        // Safe to unwrap.
        let row_state = probe_state.row_state.as_mut().unwrap();
        // The row_state_indexes[idx] = i records the row_state[i] has been increased 1 by the idx,
        // if idx is filtered by other conditions, we will set row_state[idx] = row_state[idx] - 1.
        // Safe to unwrap.
        let row_state_indexes = probe_state.row_state_indexes.as_mut().unwrap();

        let mut matched_num = 0;
        let mut probe_indexes_occupied = 0;
        let mut result_blocks = vec![];

        let data_blocks = self.row_space.chunks.read();
        let data_blocks = data_blocks
            .iter()
            .map(|c| &c.data_block)
            .collect::<Vec<_>>();
        let build_num_rows = data_blocks
            .iter()
            .fold(0, |acc, chunk| acc + chunk.num_rows());
        let is_build_projected = self.is_build_projected.load(Ordering::Relaxed);
        let outer_scan_map = unsafe { &mut *self.outer_scan_map.get() };

        // Start to probe hash table.
        for (i, key) in keys_iter.enumerate() {
            let (mut probe_matched, mut incomplete_ptr) =
                if self.hash_join_desc.from_correlated_subquery {
                    hash_table.probe_hash_table(
                        key,
                        local_build_indexes_ptr,
                        matched_num,
                        max_block_size,
                    )
                } else {
                    self.probe_key(
                        hash_table,
                        key,
                        valids,
                        i,
                        local_build_indexes_ptr,
                        matched_num,
                        max_block_size,
                    )
                };
            let mut total_probe_matched = 0;
            if probe_matched > 0 {
                total_probe_matched += probe_matched;
                if self.hash_join_desc.join_type == JoinType::LeftSingle && total_probe_matched > 1
                {
                    return Err(ErrorCode::Internal(
                        "Scalar subquery can't return more than one row",
                    ));
                }

                row_state[i] += probe_matched;
                for _ in 0..probe_matched {
                    row_state_indexes[matched_num] = i;
                    matched_num += 1;
                }
                probe_indexes[probe_indexes_occupied] = (i as u32, probe_matched as u32);
                probe_indexes_occupied += 1;
            }
            if matched_num >= max_block_size || i == input_num_rows - 1 {
                loop {
                    if self.interrupt.load(Ordering::Relaxed) {
                        return Err(ErrorCode::AbortedQuery(
                            "Aborted query, because the server is shutting down or the query was killed.",
                        ));
                    }

                    let probe_block = if is_probe_projected {
                        let mut probe_block = DataBlock::take_compacted_indices(
                            input,
                            &probe_indexes[0..probe_indexes_occupied],
                            matched_num,
                        )?;
                        // For full join, wrap nullable for probe block
                        if self.hash_join_desc.join_type == JoinType::Full {
                            let nullable_probe_columns = if matched_num == max_block_size {
                                probe_block
                                    .columns()
                                    .iter()
                                    .map(|c| Self::set_validity(c, max_block_size, true_validity))
                                    .collect::<Vec<_>>()
                            } else {
                                let mut validity = MutableBitmap::new();
                                validity.extend_constant(matched_num, true);
                                let validity: Bitmap = validity.into();
                                probe_block
                                    .columns()
                                    .iter()
                                    .map(|c| Self::set_validity(c, matched_num, &validity))
                                    .collect::<Vec<_>>()
                            };
                            probe_block = DataBlock::new(nullable_probe_columns, matched_num)
                        }
                        Some(probe_block)
                    } else {
                        None
                    };
                    let build_block = if is_build_projected {
                        let build_block = self.row_space.gather(
                            &local_build_indexes[0..matched_num],
                            &data_blocks,
                            &build_num_rows,
                        )?;
                        // For left join, wrap nullable for build block
                        let (nullable_columns, num_rows) = if build_num_rows == 0 {
                            (
                                build_block
                                    .columns()
                                    .iter()
                                    .map(|c| BlockEntry {
                                        value: Value::Scalar(Scalar::Null),
                                        data_type: c.data_type.wrap_nullable(),
                                    })
                                    .collect::<Vec<_>>(),
                                matched_num,
                            )
                        } else if matched_num == max_block_size {
                            (
                                build_block
                                    .columns()
                                    .iter()
                                    .map(|c| Self::set_validity(c, max_block_size, true_validity))
                                    .collect::<Vec<_>>(),
                                max_block_size,
                            )
                        } else {
                            let mut validity = MutableBitmap::new();
                            validity.extend_constant(matched_num, true);
                            let validity: Bitmap = validity.into();
                            (
                                build_block
                                    .columns()
                                    .iter()
                                    .map(|c| Self::set_validity(c, matched_num, &validity))
                                    .collect::<Vec<_>>(),
                                matched_num,
                            )
                        };
                        Some(DataBlock::new(nullable_columns, num_rows))
                    } else {
                        None
                    };
                    let result_block = self.merge_eq_block(probe_block, build_block, matched_num);

                    if !result_block.is_empty() {
                        let (bm, all_true, all_false) = self.get_other_filters(
                            &result_block,
                            self.hash_join_desc.other_predicate.as_ref().unwrap(),
                        )?;

                        if all_true {
                            result_blocks.push(result_block);
                            if self.hash_join_desc.join_type == JoinType::Full {
                                for row_ptr in local_build_indexes.iter().take(matched_num) {
                                    outer_scan_map[row_ptr.chunk_index as usize]
                                        [row_ptr.row_index as usize] = true;
                                }
                            }
                        } else if all_false {
                            let mut idx = 0;
                            while idx < matched_num {
                                row_state[row_state_indexes[idx]] -= 1;
                                idx += 1;
                            }
                        } else {
                            // Safe to unwrap.
                            let validity = bm.unwrap();
                            if self.hash_join_desc.join_type == JoinType::Full {
                                let mut idx = 0;
                                while idx < matched_num {
                                    let valid = unsafe { validity.get_bit_unchecked(idx) };
                                    if valid {
                                        outer_scan_map
                                            [local_build_indexes[idx].chunk_index as usize]
                                            [local_build_indexes[idx].row_index as usize] = true;
                                    } else {
                                        row_state[row_state_indexes[idx]] -= 1;
                                    }
                                    idx += 1;
                                }
                            } else {
                                let mut idx = 0;
                                while idx < matched_num {
                                    let valid = unsafe { validity.get_bit_unchecked(idx) };
                                    if !valid {
                                        row_state[row_state_indexes[idx]] -= 1;
                                    }
                                    idx += 1;
                                }
                            }
                            let filtered_block =
                                DataBlock::filter_with_bitmap(result_block, &validity)?;
                            result_blocks.push(filtered_block);
                        }
                    }

                    matched_num = 0;
                    probe_indexes_occupied = 0;

                    if incomplete_ptr == 0 {
                        break;
                    }
                    (probe_matched, incomplete_ptr) = hash_table.next_incomplete_ptr(
                        key,
                        incomplete_ptr,
                        local_build_indexes_ptr,
                        matched_num,
                        max_block_size,
                    );

                    if probe_matched > 0 {
                        total_probe_matched += probe_matched;
                        if self.hash_join_desc.join_type == JoinType::LeftSingle
                            && total_probe_matched > 1
                        {
                            return Err(ErrorCode::Internal(
                                "Scalar subquery can't return more than one row",
                            ));
                        }

                        row_state[i] += probe_matched;
                        for _ in 0..probe_matched {
                            row_state_indexes[matched_num] = i;
                            matched_num += 1;
                        }
                        probe_indexes[probe_indexes_occupied] = (i as u32, probe_matched as u32);
                        probe_indexes_occupied += 1;
                    }

                    if matched_num < max_block_size && i != input_num_rows - 1 {
                        break;
                    }
                }
            }
        }

        probe_indexes_occupied = 0;
        let mut idx = 0;
        while idx < input_num_rows {
            if row_state[idx] == 0 {
                probe_indexes[probe_indexes_occupied] = (idx as u32, 1);
                probe_indexes_occupied += 1;
                if probe_indexes_occupied >= max_block_size {
                    result_blocks.push(self.create_left_join_null_block(
                        input,
                        probe_indexes,
                        probe_indexes_occupied,
                        is_probe_projected,
                        is_build_projected,
                    )?);
                    probe_indexes_occupied = 0;
                }
            }
            row_state[idx] = 0;
            idx += 1;
        }

        if probe_indexes_occupied == 0 {
            return Ok(result_blocks);
        }
        result_blocks.push(self.create_left_join_null_block(
            input,
            probe_indexes,
            probe_indexes_occupied,
            is_probe_projected,
            is_build_projected,
        )?);
        Ok(result_blocks)
    }

    fn create_left_join_null_block(
        &self,
        input: &DataBlock,
        indexes: &[(u32, u32)],
        occupied: usize,
        is_probe_projected: bool,
        is_build_projected: bool,
    ) -> Result<DataBlock> {
        let probe_block = if is_probe_projected {
            let mut probe_block =
                DataBlock::take_compacted_indices(input, &indexes[0..occupied], occupied)?;
            // For full join, wrap nullable for probe block
            if self.hash_join_desc.join_type == JoinType::Full {
                let nullable_probe_columns = probe_block
                    .columns()
                    .iter()
                    .map(|c| {
                        let mut probe_validity = MutableBitmap::new();
                        probe_validity.extend_constant(occupied, true);
                        let probe_validity: Bitmap = probe_validity.into();
                        Self::set_validity(c, occupied, &probe_validity)
                    })
                    .collect::<Vec<_>>();
                probe_block = DataBlock::new(nullable_probe_columns, occupied);
            }
            Some(probe_block)
        } else {
            None
        };
        let build_block = if is_build_projected {
            let null_build_block = DataBlock::new(
                self.row_space
                    .build_schema
                    .fields()
                    .iter()
                    .map(|df| BlockEntry {
                        data_type: df.data_type().clone(),
                        value: Value::Scalar(Scalar::Null),
                    })
                    .collect(),
                occupied,
            );
            Some(null_build_block)
        } else {
            None
        };
        Ok(self.merge_eq_block(probe_block, build_block, occupied))
    }
}
