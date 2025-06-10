use std::sync::Arc;

use serde_json::json;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};

pub fn get_bootstrap_data() -> Vec<SourceChange> {
    vec![
        // === Products ===
        // Product 1: Laptop
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Product", "p1"),
                    labels: Arc::new([Arc::from("products")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "productId": "p1",
                    "productName": "Laptop",
                    "productDescription": "High-end gaming laptop",
                    "price": 999.99
                })),
            },
        },
        // Product 2: Mouse
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Product", "p2"),
                    labels: Arc::new([Arc::from("products")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "productId": "p2",
                    "productName": "Gaming Mouse",
                    "productDescription": "RGB gaming mouse with high DPI",
                    "price": 79.99
                })),
            },
        },
        // Product 3: Keyboard (product with no reviews for testing)
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Product", "p3"),
                    labels: Arc::new([Arc::from("products")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "productId": "p3",
                    "productName": "Mechanical Keyboard",
                    "productDescription": "RGB mechanical keyboard",
                    "price": 149.99
                })),
            },
        },
        // === Order Items ===
        // Product 1 (Laptop) has 3 orders
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("OrderItem", "oi1"),
                    labels: Arc::new([Arc::from("orderItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderItemId": "oi1",
                    "quantity": 1
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("OrderItem", "oi2"),
                    labels: Arc::new([Arc::from("orderItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderItemId": "oi2",
                    "quantity": 2
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("OrderItem", "oi3"),
                    labels: Arc::new([Arc::from("orderItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderItemId": "oi3",
                    "quantity": 1
                })),
            },
        },
        // Product 2 (Mouse) has 2 orders
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("OrderItem", "oi4"),
                    labels: Arc::new([Arc::from("orderItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderItemId": "oi4",
                    "quantity": 3
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("OrderItem", "oi5"),
                    labels: Arc::new([Arc::from("orderItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderItemId": "oi5",
                    "quantity": 5
                })),
            },
        },
        // Product 3 (Keyboard) has 1 order
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("OrderItem", "oi6"),
                    labels: Arc::new([Arc::from("orderItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderItemId": "oi6",
                    "quantity": 1
                })),
            },
        },
        // === Reviews ===
        // Product 1 (Laptop) has 3 reviews
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Review", "r1"),
                    labels: Arc::new([Arc::from("reviews")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "reviewId": "r1",
                    "rating": 5.0,
                    "comment": "Excellent laptop!"
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Review", "r2"),
                    labels: Arc::new([Arc::from("reviews")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "reviewId": "r2",
                    "rating": 4.0,
                    "comment": "Good performance, bit pricey"
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Review", "r3"),
                    labels: Arc::new([Arc::from("reviews")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "reviewId": "r3",
                    "rating": 4.5,
                    "comment": "Great for gaming"
                })),
            },
        },
        // Product 2 (Mouse) has 3 reviews
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Review", "r4"),
                    labels: Arc::new([Arc::from("reviews")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "reviewId": "r4",
                    "rating": 4.5,
                    "comment": "Smooth and responsive"
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Review", "r5"),
                    labels: Arc::new([Arc::from("reviews")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "reviewId": "r5",
                    "rating": 3.5,
                    "comment": "Good but RGB software is buggy"
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Review", "r6"),
                    labels: Arc::new([Arc::from("reviews")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "reviewId": "r6",
                    "rating": 4.0,
                    "comment": "Worth the price"
                })),
            },
        },
        // Product 3 (Keyboard) has 0 reviews - for testing WHERE reviewCount > 0

        // === Edges: Product -> OrderItem ===
        // Laptop orders
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("PRODUCT_TO_ORDER_ITEM", "p1->oi1"),
                    labels: Arc::new([Arc::from("PRODUCT_TO_ORDER_ITEM")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Product", "p1"),
                out_node: ElementReference::new("OrderItem", "oi1"),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("PRODUCT_TO_ORDER_ITEM", "p1->oi2"),
                    labels: Arc::new([Arc::from("PRODUCT_TO_ORDER_ITEM")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Product", "p1"),
                out_node: ElementReference::new("OrderItem", "oi2"),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("PRODUCT_TO_ORDER_ITEM", "p1->oi3"),
                    labels: Arc::new([Arc::from("PRODUCT_TO_ORDER_ITEM")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Product", "p1"),
                out_node: ElementReference::new("OrderItem", "oi3"),
            },
        },
        // Mouse orders
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("PRODUCT_TO_ORDER_ITEM", "p2->oi4"),
                    labels: Arc::new([Arc::from("PRODUCT_TO_ORDER_ITEM")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Product", "p2"),
                out_node: ElementReference::new("OrderItem", "oi4"),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("PRODUCT_TO_ORDER_ITEM", "p2->oi5"),
                    labels: Arc::new([Arc::from("PRODUCT_TO_ORDER_ITEM")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Product", "p2"),
                out_node: ElementReference::new("OrderItem", "oi5"),
            },
        },
        // Keyboard order
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("PRODUCT_TO_ORDER_ITEM", "p3->oi6"),
                    labels: Arc::new([Arc::from("PRODUCT_TO_ORDER_ITEM")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Product", "p3"),
                out_node: ElementReference::new("OrderItem", "oi6"),
            },
        },
        // === Edges: Review -> Product ===
        // Laptop reviews
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("REVIEW_TO_PRODUCT", "r1->p1"),
                    labels: Arc::new([Arc::from("REVIEW_TO_PRODUCT")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Review", "r1"),
                out_node: ElementReference::new("Product", "p1"),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("REVIEW_TO_PRODUCT", "r2->p1"),
                    labels: Arc::new([Arc::from("REVIEW_TO_PRODUCT")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Review", "r2"),
                out_node: ElementReference::new("Product", "p1"),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("REVIEW_TO_PRODUCT", "r3->p1"),
                    labels: Arc::new([Arc::from("REVIEW_TO_PRODUCT")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Review", "r3"),
                out_node: ElementReference::new("Product", "p1"),
            },
        },
        // Mouse reviews
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("REVIEW_TO_PRODUCT", "r4->p2"),
                    labels: Arc::new([Arc::from("REVIEW_TO_PRODUCT")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Review", "r4"),
                out_node: ElementReference::new("Product", "p2"),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("REVIEW_TO_PRODUCT", "r5->p2"),
                    labels: Arc::new([Arc::from("REVIEW_TO_PRODUCT")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Review", "r5"),
                out_node: ElementReference::new("Product", "p2"),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("REVIEW_TO_PRODUCT", "r6->p2"),
                    labels: Arc::new([Arc::from("REVIEW_TO_PRODUCT")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("Review", "r6"),
                out_node: ElementReference::new("Product", "p2"),
            },
        },
    ]
}

// Summary of test data:
// Product 1 (Laptop): 3 orders (quantities: 1, 2, 1), 3 reviews (ratings: 5.0, 4.0, 4.5)
//   - Expected: orderCount=3, avgQuantity=1.33, reviewCount=3, avgRating=4.5
// Product 2 (Mouse): 2 orders (quantities: 3, 5), 3 reviews (ratings: 4.5, 3.5, 4.0)
//   - Expected: orderCount=2, avgQuantity=4.0, reviewCount=3, avgRating=4.0
// Product 3 (Keyboard): 1 order (quantity: 1), 0 reviews
//   - Should be filtered out by WHERE reviewCount > 0
