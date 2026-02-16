// Copyright 2026 The Drasi Authors.
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

/// Basic collect_list() usage for aggregating values into lists
pub fn collect_based_aggregation_query() -> &'static str {
    "
    MATCH 
        (p:products)-[:PRODUCT_TO_ORDER_ITEM]->(oi:orderItem)
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        p.productDescription AS product_description,
        collect_list(oi.quantity) AS order_quantities
    "
}

/// Collect with filtering
pub fn collect_with_filter_query() -> &'static str {
    "
    MATCH (p:products)<-[:REVIEW_TO_PRODUCT]-(r:reviews)
    WHERE r.rating >= 4.0
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        collect_list(r.rating) as high_ratings
    "
}

/// Collect complex objects into lists
pub fn collect_objects_query() -> &'static str {
    "
    MATCH (p:products)<-[:REVIEW_TO_PRODUCT]-(r:reviews)
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        collect_list({rating: r.rating, comment: r.comment}) as review_details
    "
}

/// Multiple collects in same WITH clause
pub fn multiple_collects_query() -> &'static str {
    "
    MATCH (p:products)<-[:REVIEW_TO_PRODUCT]-(r:reviews)
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        collect_list(r.rating) as ratings,
        collect_list(r.reviewId) as review_ids,
        count(r) as review_count
    "
}
