// Copyright 2025 The Drasi Authors.
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

/// Basic collect() usage for aggregating values into lists
pub fn collect_based_aggregation_query() -> &'static str {
    "
    MATCH 
        (p:products)-[:PRODUCT_TO_ORDER_ITEM]->(oi:orderItem)
    WITH p, collect(oi.quantity) as order_quantities
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        p.productDescription AS product_description,
        order_quantities,
        size(order_quantities) as order_count
    "
}

/// Simple single-level aggregation query
pub fn simple_product_aggregation_query() -> &'static str {
    "
    MATCH
        (p:products)<-[:REVIEW_TO_PRODUCT]-(r:reviews)
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        count(r) AS review_count,
        avg(r.rating) AS avg_rating
    "
}

/// Query for product order statistics
pub fn product_order_stats_query() -> &'static str {
    "
    MATCH
        (p:products)-[:PRODUCT_TO_ORDER_ITEM]->(oi:orderItem)
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        count(oi) AS order_count,
        avg(oi.quantity) AS avg_quantity
    "
}

pub fn product_review_stats_query() -> &'static str {
    "
    MATCH
        (p:products)<-[:REVIEW_TO_PRODUCT]-(r:reviews)
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        count(r) AS review_count,
        avg(r.rating) AS avg_rating
    "
}

/// Comprehensive collect() usage with multiple relationships
/// Demonstrates advanced aggregation patterns with OPTIONAL MATCH
pub fn collect_full_solution_query() -> &'static str {
    "
    MATCH (p:products)
    OPTIONAL MATCH (p)-[:PRODUCT_TO_ORDER_ITEM]->(oi:orderItem)
    WITH p, collect(oi.quantity) as order_quantities
    OPTIONAL MATCH (r:reviews)-[:REVIEW_TO_PRODUCT]->(p)
    WITH p, order_quantities, collect(r.rating) as review_ratings
    WHERE size(order_quantities) > 0 OR size(review_ratings) > 0
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        p.productDescription AS product_description,
        size(order_quantities) AS order_count,
        CASE 
            WHEN size(order_quantities) > 0 
            THEN reduce(sum = 0.0, q IN order_quantities | sum + q) / size(order_quantities)
            ELSE null
        END AS avg_quantity,
        size(review_ratings) AS review_count,
        CASE 
            WHEN size(review_ratings) > 0 
            THEN reduce(sum = 0.0, r IN review_ratings | sum + r) / size(review_ratings)
            ELSE null
        END AS avg_rating
    "
}

/// Collect with filtering
pub fn collect_with_filter_query() -> &'static str {
    "
    MATCH (p:products)<-[:REVIEW_TO_PRODUCT]-(r:reviews)
    WHERE r.rating >= 4.0
    WITH p, collect(r.rating) as high_ratings
    WHERE size(high_ratings) >= 2
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        high_ratings,
        size(high_ratings) as high_rating_count
    "
}

/// Collect complex objects into lists
pub fn collect_objects_query() -> &'static str {
    "
    MATCH (p:products)<-[:REVIEW_TO_PRODUCT]-(r:reviews)
    WITH p, collect({rating: r.rating, comment: r.comment}) as review_details
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        review_details,
        size(review_details) as review_count
    "
}

/// Collect with mixed types
pub fn collect_mixed_types_query() -> &'static str {
    "
    MATCH (p:products)
    OPTIONAL MATCH (p)-[:PRODUCT_TO_ORDER_ITEM]->(oi:orderItem)
    OPTIONAL MATCH (r:reviews)-[:REVIEW_TO_PRODUCT]->(p)
    WITH p, collect(oi.orderItemId) as order_ids, collect(r.rating) as ratings
    WHERE size(order_ids) > 0 OR size(ratings) > 0
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        order_ids,
        ratings
    "
}

/// Multiple collects in same WITH clause
pub fn multiple_collects_query() -> &'static str {
    "
    MATCH (p:products)<-[:REVIEW_TO_PRODUCT]-(r:reviews)
    WITH 
        p, 
        collect(r.rating) as ratings,
        collect(r.reviewId) as review_ids,
        count(r) as review_count
    RETURN
        p.productId AS product_id,
        p.productName AS product_name,
        ratings,
        review_ids,
        review_count
    "
}
