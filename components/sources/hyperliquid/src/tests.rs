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

use crate::config::{CoinSelection, HyperliquidNetwork, HyperliquidSourceConfig, InitialCursor};
use crate::mapping::{
    map_funding_rate_to_changes, map_liquidation_to_changes, map_mid_prices_to_changes,
    map_order_book_to_changes, map_trade_to_changes, InitializedEntities,
};
use crate::types::{AssetCtx, L2Book, L2Level, Liquidation, Trade};
use drasi_core::models::{Element, ElementValue, SourceChange};
use ordered_float::OrderedFloat;
use std::collections::HashMap;

#[test]
fn test_network_urls() {
    let mainnet = HyperliquidNetwork::Mainnet;
    assert_eq!(mainnet.rest_url(), "https://api.hyperliquid.xyz/info");
    assert_eq!(mainnet.ws_url(), "wss://api.hyperliquid.xyz/ws");

    let testnet = HyperliquidNetwork::Testnet;
    assert_eq!(
        testnet.rest_url(),
        "https://api.hyperliquid-testnet.xyz/info"
    );
    assert_eq!(testnet.ws_url(), "wss://api.hyperliquid-testnet.xyz/ws");

    let custom = HyperliquidNetwork::Custom {
        rest_url: "https://example.com/info".to_string(),
        ws_url: "wss://example.com/ws".to_string(),
    };
    assert_eq!(custom.rest_url(), "https://example.com/info");
    assert_eq!(custom.ws_url(), "wss://example.com/ws");
}

#[test]
fn test_config_defaults() {
    let config = HyperliquidSourceConfig::default();
    assert_eq!(config.initial_cursor, InitialCursor::StartFromNow);
    assert!(matches!(config.coins, CoinSelection::All));
}

#[test]
fn test_trade_mapping() {
    let trade = Trade {
        coin: "BTC".to_string(),
        side: "B".to_string(),
        px: "65000.5".to_string(),
        sz: "0.25".to_string(),
        time: 1_700_000_000_000,
        hash: Some("0xabc".to_string()),
        tid: 42,
    };

    let changes = map_trade_to_changes("hl", &trade).unwrap();
    assert_eq!(changes.len(), 2);

    match &changes[0] {
        SourceChange::Insert { element } => match element {
            Element::Node {
                metadata,
                properties,
            } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "trade:BTC:42");
                assert_eq!(properties["coin"], ElementValue::String("BTC".into()));
                assert_eq!(
                    properties["price"],
                    ElementValue::Float(OrderedFloat(65000.5))
                );
            }
            _ => panic!("Expected Trade node"),
        },
        _ => panic!("Expected trade insert"),
    }

    match &changes[1] {
        SourceChange::Insert { element } => match element {
            Element::Relation { metadata, .. } => {
                assert_eq!(
                    metadata.reference.element_id.as_ref(),
                    "traded_on:trade:BTC:42"
                );
            }
            _ => panic!("Expected TRADED_ON relation"),
        },
        _ => panic!("Expected relation insert"),
    }
}

#[test]
fn test_midprice_insert_update() {
    let mut mids = HashMap::new();
    mids.insert("BTC".to_string(), "100.0".to_string());

    let mut initialized = InitializedEntities::new();
    let first = map_mid_prices_to_changes("hl", &mids, &mut initialized, 1000).unwrap();
    assert_eq!(first.len(), 2);

    let second = map_mid_prices_to_changes("hl", &mids, &mut initialized, 2000).unwrap();
    assert_eq!(second.len(), 1);
}

#[test]
fn test_order_book_insert_update() {
    let book = L2Book {
        coin: "BTC".to_string(),
        time: 1_700_000_000_000,
        levels: vec![
            vec![L2Level {
                px: "99.0".to_string(),
                sz: "1.0".to_string(),
                n: 1,
            }],
            vec![L2Level {
                px: "101.0".to_string(),
                sz: "2.0".to_string(),
                n: 1,
            }],
        ],
    };

    let mut initialized = InitializedEntities::new();
    let first = map_order_book_to_changes("hl", &book, &mut initialized).unwrap();
    assert_eq!(first.len(), 2);

    let second = map_order_book_to_changes("hl", &book, &mut initialized).unwrap();
    assert_eq!(second.len(), 1);
}

#[test]
fn test_liquidation_mapping() {
    let liquidation = Liquidation {
        coin: "ETH".to_string(),
        side: "A".to_string(),
        px: "2200.0".to_string(),
        sz: "3.0".to_string(),
        time: 1_700_000_000_100,
        hash: None,
    };

    let changes = map_liquidation_to_changes("hl", &liquidation).unwrap();
    assert_eq!(changes.len(), 2);

    // Without hash, ID uses timestamp
    match &changes[0] {
        SourceChange::Insert { element } => match element {
            Element::Node { metadata, .. } => {
                assert_eq!(
                    metadata.reference.element_id.as_ref(),
                    "liquidation:ETH:1700000000100"
                );
            }
            _ => panic!("Expected Liquidation node"),
        },
        _ => panic!("Expected liquidation insert"),
    }
}

#[test]
fn test_liquidation_id_uses_hash_when_present() {
    let liquidation = Liquidation {
        coin: "ETH".to_string(),
        side: "A".to_string(),
        px: "2200.0".to_string(),
        sz: "3.0".to_string(),
        time: 1_700_000_000_100,
        hash: Some("0xdeadbeef".to_string()),
    };

    let changes = map_liquidation_to_changes("hl", &liquidation).unwrap();
    assert_eq!(changes.len(), 2);

    // With hash, ID uses hash instead of timestamp to avoid collisions
    match &changes[0] {
        SourceChange::Insert { element } => match element {
            Element::Node { metadata, .. } => {
                assert_eq!(
                    metadata.reference.element_id.as_ref(),
                    "liquidation:ETH:0xdeadbeef"
                );
            }
            _ => panic!("Expected Liquidation node"),
        },
        _ => panic!("Expected liquidation insert"),
    }
}

#[test]
fn test_config_validation_no_channels_enabled() {
    let config = HyperliquidSourceConfig {
        enable_trades: false,
        enable_order_book: false,
        enable_mid_prices: false,
        enable_funding_rates: false,
        enable_liquidations: false,
        ..Default::default()
    };
    let result = config.validate();
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("at least one data channel"));
}

#[test]
fn test_funding_mapping() {
    let ctx = AssetCtx {
        funding: "0.0001".to_string(),
        open_interest: "100".to_string(),
        prev_day_px: "0".to_string(),
        day_ntl_vlm: "1000".to_string(),
        premium: "0.01".to_string(),
        oracle_px: "1".to_string(),
        mark_px: "2".to_string(),
        mid_px: "2".to_string(),
        impact_pxs: vec!["1".to_string(), "2".to_string()],
        day_base_vlm: "0".to_string(),
    };

    let mut initialized = InitializedEntities::new();
    let (changes, snapshot) =
        map_funding_rate_to_changes("hl", "BTC", &ctx, &mut initialized, 123).unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(snapshot.rate, 0.0001);

    let (changes, _) =
        map_funding_rate_to_changes("hl", "BTC", &ctx, &mut initialized, 124).unwrap();
    assert_eq!(changes.len(), 1);
}
