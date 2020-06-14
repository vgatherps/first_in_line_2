#![recursion_limit = "512"]

//WARNING
//WARNING
//
//I would not abide by all of the given practices in here in production quality trading code

use exchange::{
    bitmex_connection, bybit_connection, coinbase_connection, huobi_connection, okex_connection,
    BybitType, HuobiType, OkexType,
};

use crate::exchange::normalized::*;
use std::io::prelude::*;

use chrono::prelude::*;
use futures::{future::FutureExt, join, select};
use std::sync::mpsc;
use structopt::StructOpt;

use std::sync::Arc;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::collections::{HashMap, HashSet};

mod args;
mod central_registry;
mod displacement;
mod ema;
mod exchange;
mod fair_value;
mod fifo_pnl;
mod local_book;
mod order_book;
mod order_manager;
mod position_manager;
mod remote_venue_aggregator;
mod tactic;

use fair_value::*;

pub static DIE: AtomicBool = AtomicBool::new(false);
pub static LOOP: AtomicUsize = AtomicUsize::new(0);

fn html_writer(filename: String, requests: mpsc::Receiver<String>) {
    while let Ok(request) = requests.recv() {
        let atomic = atomicwrites::AtomicFile::new(
            &filename,
            atomicwrites::OverwriteBehavior::AllowOverwrite,
        );
        atomic
            .write(|temp_file| temp_file.write_all(request.as_bytes()))
            .expect("Couldn't write html");
    }
    println!("Done writing html");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run())
}

pub enum TacticInternalEvent {
    OrderCanceled(bitmex_http::OrderCanceled),
    Trades(SmallVec<bitmex_http::Transaction>),
    SetLateStatus(Side, usize, usize),
    CancelStale(Side, usize, usize),
    CheckGone(Side, usize, usize),
    DisplayHtml,
    Ping,
    Reset(bool),
}

enum TacticEventType {
    RemoteFair,
    LocalBook(SmallVec<local_book::InsideOrder>),
    Trades(SmallVec<bitmex_http::Transaction>),
    AckCancel(bitmex_http::OrderCanceled),
    CancelStale(Side, usize, usize),
    CheckGone(Side, usize, usize),
    SetLateStatus(Side, usize, usize),
    WriteHtml,
    Ping,
    Reset,
}

async fn reset_loop(mut event_queue: tokio::sync::mpsc::Sender<TacticInternalEvent>) {
    tokio::time::delay_for(std::time::Duration::from_millis(1000 * 60 * 10)).await;
    assert!(event_queue
        .send(TacticInternalEvent::Reset(false))
        .await
        .is_ok());
}

async fn ping_loop(mut event_queue: tokio::sync::mpsc::Sender<TacticInternalEvent>) {
    loop {
        tokio::time::delay_for(std::time::Duration::from_millis(1000 * 30)).await;
        assert!(event_queue.send(TacticInternalEvent::Ping).await.is_ok());
    }
}

async fn html_writer_loop(mut event_queue: tokio::sync::mpsc::Sender<TacticInternalEvent>) {
    loop {
        tokio::time::delay_for(std::time::Duration::from_millis(1000)).await;
        assert!(event_queue
            .send(TacticInternalEvent::DisplayHtml)
            .await
            .is_ok());
    }
}

async fn get_max_timestamp(
    last_seen: Option<String>,
    http: Arc<bitmex_http::BitmexHttp>,
) -> (String, SmallVec<bitmex_http::Transaction>) {
    let transactions = http
        .request_transactions_from(last_seen.clone(), http.clone())
        .await;
    let last_seen_filter = if let Some(ls) = last_seen {
        ls
    } else {
        String::new()
    };
    // We must reverse the transactions to go from oldest to newest
    let transactions: SmallVec<_> = transactions
        .into_iter()
        .rev()
        .filter(|t| t.timestamp > last_seen_filter)
        .collect();
    let last_seen = transactions
        .iter()
        .map(|t| t.timestamp.clone())
        .max()
        .unwrap_or(last_seen_filter);
    (last_seen, transactions)
}

async fn transaction_loop(
    mut last_seen: String,
    http: Arc<bitmex_http::BitmexHttp>,
    mut event_queue: tokio::sync::mpsc::Sender<TacticInternalEvent>,
) {
    let myloop = LOOP.load(Ordering::Relaxed);
    let mut seen = HashSet::new();
    let mut seen_qty = HashMap::new();
    while myloop == LOOP.load(Ordering::Relaxed) {
        tokio::time::delay_for(std::time::Duration::from_millis(1000 * 5)).await;
        let (new_last_seen, transactions) =
            get_max_timestamp(Some(last_seen.clone()), http.clone()).await;
        last_seen = new_last_seen;
        let mut transactions: SmallVec<_> = transactions
            .into_iter()
            .filter(|t| !seen.contains(&t.exec_id))
            .collect();
        for transaction in &mut transactions {
            seen.insert(transaction.exec_id.clone());
            let current_cum_qty = seen_qty.entry(transaction.order_id).or_insert(0usize);
            assert!(*current_cum_qty <= transaction.cum_size);
            transaction.size = transaction.cum_size - *current_cum_qty;
            *current_cum_qty = transaction.cum_size;
        }
        if transactions.len() > 0 {
            assert!(event_queue
                .send(TacticInternalEvent::Trades(transactions))
                .await
                .is_ok());
        }
    }
    println!("Exiting transaction loop");
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let start = Local::now();
    let args = args::Arguments::from_args();
    let (html_queue, html_reader) = std::sync::mpsc::channel();

    //http state lives outside of the loop to properly rate-limit requests
    let http = bitmex_http::BitmexHttp::new(args.auth_key, args.auth_secret);
    let http = Arc::new(http);

    let html = args.html.clone();
    std::thread::spawn(move || html_writer(html, html_reader));

    let test_position = position_manager::PositionManager::create(http.clone()).await;

    let mut statistics = tactic::TacticStatistics::new();

    drop(test_position);

    let mut bad_runs_count: usize = 0;
    loop {
        // This is a little weird. We need to 'kill this', but actually dropping it poisons
        // the various events pushing into it. So instead, this lives outside the data loop scope,
        // and at the end of each iteration a task is spawned (read: leaked) to consume and drop
        // all incoming messages
        let (event_queue, mut event_reader) = tokio::sync::mpsc::channel(100);
        {
            let (md_sender, md_receiver) = bounded(5000);
            let desired_indices: Vec<_> = securities
                .iter()
                .map(|s| sec_map.to_index(s).unwrap())
                .collect();
            let md_map = sec_map.clone();
            let md_thread = std::thread::spawn(move || {
                md_thread::start_md_thread(md_sender, desired_indices, md_map)
            });

            // Spawn all tasks after we've connected to everything
            tokio::task::spawn(html_writer_loop(event_queue.clone()));
            tokio::task::spawn(reset_loop(event_queue.clone()));
            tokio::task::spawn(ping_loop(event_queue.clone()));
            tokio::task::spawn(transaction_loop(
                get_max_timestamp(None, http.clone()).await.0,
                http.clone(),
                event_queue.clone(),
            ));

            loop {
                if DIE.load(Ordering::Relaxed) {
                    panic!("Death variable set");
                }
                let event_type = match md_receiver.try_recv() {
                    Ok((index, data)) => None,
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => panic!("Market data disconnected"),
                };
                let event_type = if let Some(event_type) = event_type {
                    event_type
                } else {
                    match event_reader.try_recv() {
                        Ok(TacticInternalEvent::DisplayHtml) => TacticEventType::WriteHtml,
                        Ok(TacticInternalEvent::Reset(is_bad)) => {
                            if is_bad {
                                bad_runs_count += 1;
                            }
                            TacticEventType::Reset
                        }
                    }
                    TacticEventType::LocalBook(_) => {
                        if let Some((_, (local_fair, local_size))) = local_book.get_local_tob() {
                            displacement.handle_local(local_fair, local_size);
                        };
                    }
                    TacticEventType::Trades(trades) => {
                        for trade in trades {
                            tactic.check_seen_trade(trade);
                        }
                    }
                    TacticEventType::SetLateStatus(side, price, id) => {
                        tactic.set_late_status(*id, *price, *side)
                    }
                    TacticEventType::CancelStale(side, price, id) => {
                        tactic.cancel_stale_id(*id, *price, *side)
                    }
                    TacticEventType::CheckGone(side, price, id) => {
                        tactic.check_order_gone(*id, *price, *side)
                    }
                    TacticEventType::AckCancel(cancel) => tactic.ack_cancel_for(cancel),
                    TacticEventType::Reset => {
                        // let in-flight items propogate
                        // We do a cancel all before we wait, and will do another after the wait
                        // In almost all cases this should get rid of in-flight orders
                        tokio::time::delay_for(std::time::Duration::from_millis(1000 * 2)).await;

                        // Do a reset
                        break;
                    }
                    TacticEventType::WriteHtml => {
                        let html = format!(
                            "
                        <!DOCYPE html>
                        <html>
                        <head>
                        <meta charset=\"UTF-8\" content-type=\"text/html\">
                        <meta name=\"description\" content=\"Bitcoin\">
                        <meta http-equiv=\"refresh\" content=\"3\" >
                        </head>
                        <body>
                        <h4>Going Since {start} </h4>
                        <h4>Last Update {now} </h4>
                        </body>
                        </html>
                        ",
                            start = start,
                            now = Local::now(),
                        );
                        html_queue.send(html).expect("Couldn't send html");
                    }
                    TacticEventType::None => tokio::task::yield_now().await,
                };
            }
        }

        // We keep the state in the destructor to ensure everything exits cleanly
        println!("Resetting time {}", bad_runs_count);
        assert!(bad_runs_count <= 5);
        LOOP.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move { while let Some(_) = event_reader.recv().await {} });
        tokio::time::delay_for(std::time::Duration::from_millis(1000 * 2)).await;
    }
}
