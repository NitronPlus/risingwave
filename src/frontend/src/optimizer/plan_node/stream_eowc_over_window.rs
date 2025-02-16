// Copyright 2023 RisingWave Labs
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

use std::collections::HashSet;

use fixedbitset::FixedBitSet;
use risingwave_common::util::sort_util::OrderType;
use risingwave_pb::stream_plan::stream_node::PbNodeBody;

use super::generic::{self, PlanWindowFunction};
use super::utils::{impl_distill_by_unit, TableCatalogBuilder};
use super::{ExprRewritable, PlanBase, PlanRef, PlanTreeNodeUnary, StreamNode};
use crate::optimizer::plan_node::stream::StreamPlanRef;
use crate::stream_fragmenter::BuildFragmentGraphState;
use crate::TableCatalog;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamEowcOverWindow {
    pub base: PlanBase,
    logical: generic::OverWindow<PlanRef>,
}

impl StreamEowcOverWindow {
    pub fn new(logical: generic::OverWindow<PlanRef>) -> Self {
        assert!(logical.funcs_have_same_partition_and_order());

        let input = &logical.input;
        assert!(input.append_only());
        assert!(input.emit_on_window_close());

        // Should order by a single watermark column.
        let order_key = &logical.window_functions[0].order_by;
        assert_eq!(order_key.len(), 1);
        assert_eq!(order_key[0].order_type, OrderType::ascending());
        let order_key_idx = order_key[0].column_index;
        let input_watermark_cols = logical.input.watermark_columns();
        assert!(input_watermark_cols.contains(order_key_idx));

        // `EowcOverWindowExecutor` cannot produce any watermark columns, because there may be some
        // ancient rows in some rarely updated partitions that are emitted at the end of time.
        let watermark_columns = FixedBitSet::with_capacity(logical.output_len());

        let base = PlanBase::new_stream_with_logical(
            &logical,
            input.distribution().clone(),
            true,
            true,
            watermark_columns,
        );
        StreamEowcOverWindow { base, logical }
    }

    fn window_functions(&self) -> &[PlanWindowFunction] {
        &self.logical.window_functions
    }

    fn partition_key_indices(&self) -> Vec<usize> {
        self.window_functions()[0]
            .partition_by
            .iter()
            .map(|i| i.index())
            .collect()
    }

    fn order_key_index(&self) -> usize {
        self.window_functions()[0].order_by[0].column_index
    }

    fn infer_state_table(&self) -> TableCatalog {
        // The EOWC over window state table has the same schema as the input.

        let in_fields = self.logical.input.schema().fields();
        let mut tbl_builder =
            TableCatalogBuilder::new(self.ctx().with_options().internal_table_subset());
        for field in in_fields {
            tbl_builder.add_column(field);
        }

        let mut order_cols = HashSet::new();
        let partition_key_indices = self.partition_key_indices();
        for idx in &partition_key_indices {
            if !order_cols.contains(idx) {
                tbl_builder.add_order_column(*idx, OrderType::ascending());
                order_cols.insert(*idx);
            }
        }
        let read_prefix_len_hint = tbl_builder.get_current_pk_len();
        let order_key_index = self.order_key_index();
        if !order_cols.contains(&order_key_index) {
            tbl_builder.add_order_column(order_key_index, OrderType::ascending());
            order_cols.insert(order_key_index);
        }
        for idx in self.logical.input.expect_stream_key() {
            if !order_cols.contains(idx) {
                tbl_builder.add_order_column(*idx, OrderType::ascending());
                order_cols.insert(*idx);
            }
        }

        let in_dist_key = self.logical.input.distribution().dist_column_indices();
        tbl_builder.build(in_dist_key.to_vec(), read_prefix_len_hint)
    }
}

impl_distill_by_unit!(StreamEowcOverWindow, logical, "StreamEowcOverWindow");

impl PlanTreeNodeUnary for StreamEowcOverWindow {
    fn input(&self) -> PlanRef {
        self.logical.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        let mut logical = self.logical.clone();
        logical.input = input;
        Self::new(logical)
    }
}
impl_plan_tree_node_for_unary! { StreamEowcOverWindow }

impl StreamNode for StreamEowcOverWindow {
    fn to_stream_prost_body(&self, state: &mut BuildFragmentGraphState) -> PbNodeBody {
        use risingwave_pb::stream_plan::*;

        let calls = self
            .window_functions()
            .iter()
            .map(PlanWindowFunction::to_protobuf)
            .collect();
        let partition_by = self.window_functions()[0]
            .partition_by
            .iter()
            .map(|i| i.index() as _)
            .collect();
        let order_by = self.window_functions()[0]
            .order_by
            .iter()
            .map(|o| o.to_protobuf())
            .collect();
        let state_table = self
            .infer_state_table()
            .with_id(state.gen_table_id_wrapped())
            .to_internal_table_prost();

        PbNodeBody::EowcOverWindow(EowcOverWindowNode {
            calls,
            partition_by,
            order_by,
            state_table: Some(state_table),
        })
    }
}
impl ExprRewritable for StreamEowcOverWindow {}
