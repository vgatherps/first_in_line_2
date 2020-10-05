use crate::ema::Ema;
use crate::exchange::normalized::*;
use crate::fair_value::FairValue;
use crate::order_book::OrderBook;
use futures::{future::FutureExt, join, select};

use horrorshow::html;

// Hardcoded because futures are a bit silly for selecting variable amounts
pub struct RemoteVenueAggregator {
    okex_spot: MarketDataStream,
    okex_swap: MarketDataStream,
    okex_quarterly: MarketDataStream,
    bybit_usdt: MarketDataStream,
    bitmex: MarketDataStream,
    ftx: MarketDataStream,
    huobi: MarketDataStream,
    books: [OrderBook; Exchange::COUNT as usize],
    fairs: [f64; Exchange::COUNT as usize],
    size_ema: [Ema; Exchange::COUNT as usize],
    valuer: FairValue,
}

impl RemoteVenueAggregator {
    pub fn new(
        okex_spot: MarketDataStream,
        okex_swap: MarketDataStream,
        okex_quarterly: MarketDataStream,
        bybit_usdt: MarketDataStream,
        bitmex: MarketDataStream,
        huobi: MarketDataStream,
        ftx: MarketDataStream,
        valuer: FairValue,
        size_ratio: f64,
    ) -> RemoteVenueAggregator {
        RemoteVenueAggregator {
            okex_spot,
            okex_swap,
            okex_quarterly,
            bybit_usdt,
            bitmex,
            ftx,
            huobi,
            valuer,
            fairs: Default::default(),
            size_ema: [Ema::new(size_ratio); Exchange::COUNT as usize],
            books: Default::default(),
        }
    }

    fn update_fair_for(&mut self, block: MarketEventBlock) {
        let book = &mut self.books[block.exchange as usize];
        for event in block.events {
            book.handle_book_event(&event);
        }
        match book.bbo() {
            (Some((bid, _)), Some((ask, _))) => {
                let new_fair = self.valuer.fair_value(book.bids(), book.asks(), (bid, ask));
                self.fairs[block.exchange as usize] = new_fair.fair_price;
                self.size_ema[block.exchange as usize].add_value(new_fair.fair_shares);
            }
            _ => (),
        }
    }

    pub fn calculate_fair(&self) -> Option<(f64, f64)> {
        let mut total_price = 0.0;
        let mut total_size = 0.0;
        for i in 0..(Exchange::COUNT as usize) {
            let size = self.size_ema[i].get_value().unwrap_or(0.0);
            assert!(size >= 0.0);
            if size < 10.0 {
                continue;
            }
            let size = match i {
                // Slow data feed
                _ if i == Exchange::HuobiSpot as usize => size * 0.7,
                _ if i == Exchange::HuobiSwap as usize => size * 0.7,
                _ if i == Exchange::HuobiQuarterly as usize => size * 0.7,
                _ if i == Exchange::Ftx as usize => size * 2.5,
                _ => size,
            };
            total_price += self.fairs[i] * size;
            total_size += size;
        }
        if total_size < 100.0 {
            return None;
        }
        Some((total_price / total_size, total_size))
    }

    // TODO think about fair spread
    pub async fn get_new_fair(&mut self) {
        select! {
            b = self.okex_spot.next().fuse() => self.update_fair_for(b),
            b = self.okex_swap.next().fuse() => self.update_fair_for(b),
            b = self.okex_quarterly.next().fuse() => self.update_fair_for(b),
            b = self.bitmex.next().fuse() => self.update_fair_for(b),
            b = self.huobi.next().fuse() => self.update_fair_for(b),
            b = self.bybit_usdt.next().fuse() => self.update_fair_for(b),
            b = self.ftx.next().fuse() => self.update_fair_for(b),
        }
    }

    pub async fn ping(&mut self) {
        let _ = join!(
            self.okex_spot.ping(),
            self.okex_swap.ping(),
            self.okex_quarterly.ping(),
            self.bitmex.ping(),
            self.huobi.ping(),
            self.bybit_usdt.ping(),
            self.ftx.ping(),
        );
    }

    pub fn get_exchange_description(&self, exch: Exchange) -> String {
        format!(
            "fair value: {:.2}, fair size: {:.0}",
            self.fairs[exch as usize],
            self.size_ema[exch as usize].get_value().unwrap_or(0.0)
        )
    }

    pub fn get_html_info(&self) -> String {
        format!(
            "{}",
            html! {
                // attributes
                h3(id="remote heading", class="title") : "Remote fair value summary";
                ul(id="Fair values") {
                    li(first?=false, class="item") {
                        : format!("OkexSpot: {}", self.get_exchange_description(Exchange::OkexSpot))
                    }
                    li(first?=false, class="item") {
                        : format!("OkexSwap: {}", self.get_exchange_description(Exchange::OkexSwap))
                    }
                    li(first?=false, class="item") {
                        : format!("OkexQuarterly: {}", self.get_exchange_description(Exchange::OkexQuarterly))
                    }

                    li(first?=false, class="item") {
                        : format!("BybitUSDT: {}", self.get_exchange_description(Exchange::BybitUSDT))
                    }
                    li(first?=false, class="item") {
                        : format!("Bitmex: {}", self.get_exchange_description(Exchange::Bitmex))
                    }
                    li(first?=false, class="item") {
                        : format!("Huobi: {}", self.get_exchange_description(Exchange::HuobiSpot))
                    }
                    li(first?=false, class="item") {
                        : format!("HuobiSwap: {}", self.get_exchange_description(Exchange::HuobiSwap))
                    }
                    li(first?=false, class="item") {
                        : format!("HuobiQuarterly: {}", self.get_exchange_description(Exchange::HuobiQuarterly))
                    }
                    li(first?=false, class="item") {
                        : format!("Ftx: {}", self.get_exchange_description(Exchange::Ftx))
                    }
                    li(first?=false, class="item") {
                        : format!("Fair value+size: {:?}", self.calculate_fair().unwrap_or((0.0, 0.0)))
                    }
                }
            }
        )
    }
}
