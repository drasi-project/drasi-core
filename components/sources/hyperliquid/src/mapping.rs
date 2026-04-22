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

//! Mapping from Hyperliquid API responses to Drasi SourceChange events.

use crate::types::{
    AssetCtx, FundingSnapshot, L2Book, Liquidation, MetaResponse, SpotMetaResponse, Trade,
};
use anyhow::{anyhow, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use ordered_float::OrderedFloat;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const COIN_LABEL: &str = "Coin";
const TRADE_LABEL: &str = "Trade";
const MIDPRICE_LABEL: &str = "MidPrice";
const ORDERBOOK_LABEL: &str = "OrderBook";
const LIQUIDATION_LABEL: &str = "Liquidation";
const FUNDING_LABEL: &str = "FundingRate";
const SPOTPAIR_LABEL: &str = "SpotPair";

const REL_TRADED_ON: &str = "TRADED_ON";
const REL_PRICE_OF: &str = "PRICE_OF";
const REL_BOOK_OF: &str = "BOOK_OF";
const REL_LIQUIDATED_ON: &str = "LIQUIDATED_ON";
const REL_FUNDING_OF: &str = "FUNDING_OF";

#[derive(Debug, Default)]
pub struct InitializedEntities {
    mid_prices: HashSet<String>,
    order_books: HashSet<String>,
    funding_rates: HashSet<String>,
}

impl InitializedEntities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_mid_price(&mut self, coin: &str) -> bool {
        self.mid_prices.insert(coin.to_string())
    }

    pub fn mark_order_book(&mut self, coin: &str) -> bool {
        self.order_books.insert(coin.to_string())
    }

    pub fn mark_funding_rate(&mut self, coin: &str) -> bool {
        self.funding_rates.insert(coin.to_string())
    }
}

pub fn build_coin_id(coin: &str) -> String {
    format!("coin:{coin}")
}

fn build_trade_id(coin: &str, tid: u64) -> String {
    format!("trade:{coin}:{tid}")
}

fn build_midprice_id(coin: &str) -> String {
    format!("midprice:{coin}")
}

fn build_orderbook_id(coin: &str) -> String {
    format!("orderbook:{coin}")
}

fn build_liquidation_id(coin: &str, timestamp: i64, hash: &Option<String>) -> String {
    if let Some(hash) = hash {
        format!("liquidation:{coin}:{hash}")
    } else {
        format!("liquidation:{coin}:{timestamp}")
    }
}

fn build_funding_id(coin: &str) -> String {
    format!("funding:{coin}")
}

fn build_spot_pair_id(name: &str) -> String {
    format!("spotpair:{name}")
}

fn build_relation_id(label: &str, source_element_id: &str) -> String {
    format!("{label}:{source_element_id}")
}

fn labels(label: &str) -> Arc<[Arc<str>]> {
    vec![Arc::from(label)].into()
}

fn element_metadata(
    source_id: &str,
    element_id: &str,
    label: &str,
    effective_from: u64,
) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(source_id, element_id),
        labels: labels(label),
        effective_from,
    }
}

fn relation_element(
    source_id: &str,
    relation_id: &str,
    label: &str,
    from_id: &str,
    to_id: &str,
    effective_from: u64,
) -> Element {
    Element::Relation {
        metadata: element_metadata(source_id, relation_id, label, effective_from),
        properties: ElementPropertyMap::new(),
        in_node: ElementReference::new(source_id, from_id),
        out_node: ElementReference::new(source_id, to_id),
    }
}

fn node_element(
    source_id: &str,
    element_id: &str,
    label: &str,
    effective_from: u64,
    properties: ElementPropertyMap,
) -> Element {
    Element::Node {
        metadata: element_metadata(source_id, element_id, label, effective_from),
        properties,
    }
}

fn insert_string(props: &mut ElementPropertyMap, key: &str, value: &str) {
    props.insert(key, ElementValue::String(Arc::from(value)));
}

fn insert_float(props: &mut ElementPropertyMap, key: &str, value: f64) {
    props.insert(key, ElementValue::Float(OrderedFloat(value)));
}

fn insert_integer(props: &mut ElementPropertyMap, key: &str, value: i64) {
    props.insert(key, ElementValue::Integer(value));
}

fn parse_f64(value: &str, field: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .map_err(|e| anyhow!("Failed to parse {field} value '{value}': {e}"))
}

fn effective_from(timestamp: i64) -> u64 {
    if timestamp <= 0 {
        0
    } else {
        timestamp as u64
    }
}

pub fn map_meta_to_coin_changes(source_id: &str, meta: &MetaResponse) -> Result<Vec<SourceChange>> {
    let mut changes = Vec::new();

    for asset in &meta.universe {
        let mut props = ElementPropertyMap::new();
        insert_string(&mut props, "name", &asset.name);
        insert_integer(&mut props, "sz_decimals", asset.sz_decimals as i64);
        insert_integer(&mut props, "max_leverage", asset.max_leverage as i64);
        insert_string(&mut props, "market_type", "perp");

        let element = node_element(source_id, &build_coin_id(&asset.name), COIN_LABEL, 0, props);
        changes.push(SourceChange::Insert { element });
    }

    Ok(changes)
}

pub fn map_spot_meta_to_nodes(
    source_id: &str,
    spot_meta: &SpotMetaResponse,
    existing_coins: &mut HashSet<String>,
) -> Result<Vec<SourceChange>> {
    let mut changes = Vec::new();

    let token_lookup: HashMap<u32, String> = spot_meta
        .tokens
        .iter()
        .map(|token| (token.index, token.name.clone()))
        .collect();

    for token in &spot_meta.tokens {
        if existing_coins.insert(token.name.clone()) {
            let mut props = ElementPropertyMap::new();
            insert_string(&mut props, "name", &token.name);
            insert_integer(&mut props, "sz_decimals", token.sz_decimals as i64);
            insert_integer(&mut props, "max_leverage", 0);
            insert_string(&mut props, "market_type", "spot");

            let element =
                node_element(source_id, &build_coin_id(&token.name), COIN_LABEL, 0, props);
            changes.push(SourceChange::Insert { element });
        }
    }

    for pair in &spot_meta.universe {
        let mut props = ElementPropertyMap::new();
        insert_string(&mut props, "name", &pair.name);

        let token_names: Vec<ElementValue> = pair
            .tokens
            .iter()
            .filter_map(|idx| token_lookup.get(idx))
            .map(|name| ElementValue::String(Arc::from(name.as_str())))
            .collect();
        props.insert("tokens", ElementValue::List(token_names));

        let element = node_element(
            source_id,
            &build_spot_pair_id(&pair.name),
            SPOTPAIR_LABEL,
            0,
            props,
        );
        changes.push(SourceChange::Insert { element });
    }

    Ok(changes)
}

pub fn map_trade_to_changes(source_id: &str, trade: &Trade) -> Result<Vec<SourceChange>> {
    let price = parse_f64(&trade.px, "trade.px")?;
    let size = parse_f64(&trade.sz, "trade.sz")?;

    let mut props = ElementPropertyMap::new();
    insert_string(&mut props, "coin", &trade.coin);
    insert_string(&mut props, "side", &trade.side);
    insert_float(&mut props, "price", price);
    insert_float(&mut props, "size", size);
    insert_integer(&mut props, "timestamp", trade.time);
    insert_integer(&mut props, "tid", trade.tid as i64);
    if let Some(hash) = &trade.hash {
        insert_string(&mut props, "hash", hash);
    }

    let trade_id = build_trade_id(&trade.coin, trade.tid);
    let trade_element = node_element(
        source_id,
        &trade_id,
        TRADE_LABEL,
        effective_from(trade.time),
        props,
    );

    let relation_id = build_relation_id("traded_on", &trade_id);
    let relation_element = relation_element(
        source_id,
        &relation_id,
        REL_TRADED_ON,
        &trade_id,
        &build_coin_id(&trade.coin),
        effective_from(trade.time),
    );

    Ok(vec![
        SourceChange::Insert {
            element: trade_element,
        },
        SourceChange::Insert {
            element: relation_element,
        },
    ])
}

pub fn map_liquidation_to_changes(
    source_id: &str,
    liquidation: &Liquidation,
) -> Result<Vec<SourceChange>> {
    let price = parse_f64(&liquidation.px, "liquidation.px")?;
    let size = parse_f64(&liquidation.sz, "liquidation.sz")?;

    let mut props = ElementPropertyMap::new();
    insert_string(&mut props, "coin", &liquidation.coin);
    insert_string(&mut props, "side", &liquidation.side);
    insert_float(&mut props, "price", price);
    insert_float(&mut props, "size", size);
    insert_integer(&mut props, "timestamp", liquidation.time);
    if let Some(hash) = &liquidation.hash {
        insert_string(&mut props, "hash", hash);
    }

    let liquidation_id =
        build_liquidation_id(&liquidation.coin, liquidation.time, &liquidation.hash);
    let liquidation_element = node_element(
        source_id,
        &liquidation_id,
        LIQUIDATION_LABEL,
        effective_from(liquidation.time),
        props,
    );

    let relation_id = build_relation_id("liquidated_on", &liquidation_id);
    let relation_element = relation_element(
        source_id,
        &relation_id,
        REL_LIQUIDATED_ON,
        &liquidation_id,
        &build_coin_id(&liquidation.coin),
        effective_from(liquidation.time),
    );

    Ok(vec![
        SourceChange::Insert {
            element: liquidation_element,
        },
        SourceChange::Insert {
            element: relation_element,
        },
    ])
}

pub fn map_mid_prices_to_changes(
    source_id: &str,
    mids: &HashMap<String, String>,
    initialized: &mut InitializedEntities,
    timestamp: i64,
) -> Result<Vec<SourceChange>> {
    let mut changes = Vec::new();

    for (coin, price_str) in mids {
        let price = parse_f64(price_str, "midprice")?;
        let mut props = ElementPropertyMap::new();
        insert_string(&mut props, "coin", coin);
        insert_float(&mut props, "price", price);
        insert_integer(&mut props, "timestamp", timestamp);

        let mid_id = build_midprice_id(coin);
        let element = node_element(
            source_id,
            &mid_id,
            MIDPRICE_LABEL,
            effective_from(timestamp),
            props,
        );

        if initialized.mark_mid_price(coin) {
            changes.push(SourceChange::Insert { element });

            let relation_id = build_relation_id("price_of", &mid_id);
            let relation_element = relation_element(
                source_id,
                &relation_id,
                REL_PRICE_OF,
                &mid_id,
                &build_coin_id(coin),
                effective_from(timestamp),
            );
            changes.push(SourceChange::Insert {
                element: relation_element,
            });
        } else {
            changes.push(SourceChange::Update { element });
        }
    }

    Ok(changes)
}

pub fn map_order_book_to_changes(
    source_id: &str,
    book: &L2Book,
    initialized: &mut InitializedEntities,
) -> Result<Vec<SourceChange>> {
    let bid_levels = match book.levels.first() {
        Some(levels) if !levels.is_empty() => levels,
        _ => return Ok(Vec::new()),
    };
    let ask_levels = match book.levels.get(1) {
        Some(levels) if !levels.is_empty() => levels,
        _ => return Ok(Vec::new()),
    };

    let best_bid = bid_levels
        .first()
        .ok_or_else(|| anyhow!("Order book missing best bid"))?;
    let best_ask = ask_levels
        .first()
        .ok_or_else(|| anyhow!("Order book missing best ask"))?;

    let mut props = ElementPropertyMap::new();
    insert_string(&mut props, "coin", &book.coin);
    insert_float(
        &mut props,
        "best_bid_price",
        parse_f64(&best_bid.px, "best_bid_price")?,
    );
    insert_float(
        &mut props,
        "best_bid_size",
        parse_f64(&best_bid.sz, "best_bid_size")?,
    );
    insert_float(
        &mut props,
        "best_ask_price",
        parse_f64(&best_ask.px, "best_ask_price")?,
    );
    insert_float(
        &mut props,
        "best_ask_size",
        parse_f64(&best_ask.sz, "best_ask_size")?,
    );
    insert_integer(&mut props, "bid_depth", bid_levels.len() as i64);
    insert_integer(&mut props, "ask_depth", ask_levels.len() as i64);
    insert_integer(&mut props, "timestamp", book.time);

    let orderbook_id = build_orderbook_id(&book.coin);
    let element = node_element(
        source_id,
        &orderbook_id,
        ORDERBOOK_LABEL,
        effective_from(book.time),
        props,
    );

    let mut changes = Vec::new();
    if initialized.mark_order_book(&book.coin) {
        changes.push(SourceChange::Insert { element });
        let relation_id = build_relation_id("book_of", &orderbook_id);
        let relation_element = relation_element(
            source_id,
            &relation_id,
            REL_BOOK_OF,
            &orderbook_id,
            &build_coin_id(&book.coin),
            effective_from(book.time),
        );
        changes.push(SourceChange::Insert {
            element: relation_element,
        });
    } else {
        changes.push(SourceChange::Update { element });
    }

    Ok(changes)
}

pub fn map_funding_rate_to_changes(
    source_id: &str,
    coin: &str,
    ctx: &AssetCtx,
    initialized: &mut InitializedEntities,
    timestamp: i64,
) -> Result<(Vec<SourceChange>, FundingSnapshot)> {
    let rate = parse_f64(&ctx.funding, "funding")?;
    let premium = ctx
        .premium
        .as_deref()
        .map(|s| parse_f64(s, "premium"))
        .transpose()?
        .unwrap_or(0.0);
    let mark_price = parse_f64(&ctx.mark_px, "mark_px")?;
    let open_interest = parse_f64(&ctx.open_interest, "open_interest")?;
    let volume_24h = parse_f64(&ctx.day_ntl_vlm, "day_ntl_vlm")?;

    let mut props = ElementPropertyMap::new();
    insert_string(&mut props, "coin", coin);
    insert_float(&mut props, "rate", rate);
    insert_float(&mut props, "premium", premium);
    insert_float(&mut props, "mark_price", mark_price);
    insert_float(&mut props, "open_interest", open_interest);
    insert_float(&mut props, "volume_24h", volume_24h);
    insert_integer(&mut props, "timestamp", timestamp);

    let funding_id = build_funding_id(coin);
    let element = node_element(
        source_id,
        &funding_id,
        FUNDING_LABEL,
        effective_from(timestamp),
        props,
    );

    let mut changes = Vec::new();
    if initialized.mark_funding_rate(coin) {
        changes.push(SourceChange::Insert { element });
        let relation_id = build_relation_id("funding_of", &funding_id);
        let relation_element = relation_element(
            source_id,
            &relation_id,
            REL_FUNDING_OF,
            &funding_id,
            &build_coin_id(coin),
            effective_from(timestamp),
        );
        changes.push(SourceChange::Insert {
            element: relation_element,
        });
    } else {
        changes.push(SourceChange::Update { element });
    }

    let snapshot = FundingSnapshot {
        rate,
        premium,
        mark_price,
        open_interest,
        volume_24h,
        timestamp,
    };

    Ok((changes, snapshot))
}
