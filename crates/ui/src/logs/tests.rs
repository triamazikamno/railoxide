use super::*;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;

fn entry(seq: u64, level: Level, target: &str, message: &str) -> LogEntry {
    LogEntry {
        seq,
        unix_ms: 0,
        level,
        target: target.into(),
        message: message.into(),
    }
}

fn rows(seqs: &[u64]) -> Vec<LogEntry> {
    seqs.iter()
        .map(|&seq| entry(seq, Level::INFO, "t", "m"))
        .collect()
}

#[test]
fn default_log_capacity_is_5000() {
    assert_eq!(DEFAULT_LOG_CAPACITY, 5_000);
}

#[test]
fn clear_keeps_sequence_counting() {
    let logs = LogStore::new(4);
    logs.push(entry(0, Level::INFO, "t", "before"));
    let rev = logs.rev();
    logs.clear();
    assert!(logs.logs().is_empty());
    assert!(logs.rev() > rev);
    logs.push(entry(0, Level::INFO, "t", "after"));
    assert_eq!(logs.logs()[0].seq, 1);
}

#[test]
fn filter_matches_levels_targets_and_hides() {
    let shown = |filter: &LogFilter, targets: &[&str]| -> Vec<String> {
        targets
            .iter()
            .filter(|target| filter.matches(&entry(0, Level::INFO, target, "m")))
            .map(|target| (*target).to_owned())
            .collect()
    };
    let targets = [
        "wallet_ops::rpc_broker",
        "wallet_ops::rpc_broker::warmup",
        "wallet_ops::rpc_broker_metrics",
        "wallet_ops::poi",
        "arti_client",
        "arti_client::status",
    ];

    let only = LogFilter::new("", None, vec!["wallet_ops::rpc_broker".into()], vec![]);
    assert_eq!(
        shown(&only, &targets),
        ["wallet_ops::rpc_broker", "wallet_ops::rpc_broker::warmup"]
    );

    let hide = LogFilter::new(
        "",
        None,
        vec![],
        vec!["arti_client".into(), "wallet_ops::poi".into()],
    );
    assert_eq!(
        shown(&hide, &targets),
        [
            "wallet_ops::rpc_broker",
            "wallet_ops::rpc_broker::warmup",
            "wallet_ops::rpc_broker_metrics"
        ]
    );

    let warn = LogFilter::new("", Some(Level::WARN), vec![], vec![]);
    let levels: Vec<Level> = [
        Level::ERROR,
        Level::WARN,
        Level::INFO,
        Level::DEBUG,
        Level::TRACE,
    ]
    .into_iter()
    .filter(|&level| warn.matches(&entry(0, level, "t", "m")))
    .collect();
    assert_eq!(levels, [Level::ERROR, Level::WARN]);

    let text = LogFilter::new("  RPC ", None, vec![], vec![]);
    assert!(text.matches(&entry(0, Level::INFO, "t", "Rpc request failed")));
    assert!(text.matches(&entry(0, Level::INFO, "wallet_ops::rpc_broker", "m")));
    assert!(!text.matches(&entry(0, Level::INFO, "t", "m")));
}

#[test]
fn copied_lines_use_rfc3339_utc_millis() {
    let mut line = entry(0, Level::WARN, "wallet_ops::rpc_broker", "slow");
    // 2026-11-05T14:03:22.123Z
    line.unix_ms = 1_793_887_402_123;
    let lines = [line.clone(), line];
    assert_eq!(
        format_log_lines(&lines),
        "2026-11-05T14:03:22.123Z WARN wallet_ops::rpc_broker: slow\n\
             2026-11-05T14:03:22.123Z WARN wallet_ops::rpc_broker: slow"
    );
    assert_eq!(format_log_messages(&lines), "slow\nslow");
    assert_eq!(format_count(4_812), "4,812");
    assert_eq!(format_count(1_000_000), "1,000,000");
    assert_eq!(format_count(999), "999");
}

#[test]
fn selection_ranges_toggles_and_prunes() {
    let rows = rows(&[10, 11, 12, 13, 14]);
    let mut selection = LogSelection::default();

    selection.click(&rows, 11, false, false);
    selection.click(&rows, 13, true, false);
    assert_eq!(selection.selected, BTreeSet::from([11, 12, 13]));

    selection.click(&rows, 12, false, true);
    assert_eq!(selection.selected, BTreeSet::from([11, 13]));

    selection.prune(Some(12));
    assert_eq!(selection.selected, BTreeSet::from([13]));
    selection.prune(Some(14));
    assert!(selection.selected.is_empty());
    assert_eq!((selection.anchor, selection.cursor), (None, None));

    assert_eq!(selection.move_cursor(&rows, false, false), Some(4));
    assert_eq!(selection.move_cursor(&rows, false, true), Some(3));
    assert_eq!(selection.selected, BTreeSet::from([13, 14]));
}

#[test]
fn new_lines_count_shown_rows_after_pause() {
    let rows = rows(&[3, 5, 8, 9]);
    assert_eq!(new_line_count(&rows, None), 0);
    assert_eq!(new_line_count(&rows, Some(6)), 2);
    assert_eq!(new_line_count(&rows, Some(10)), 0);
}

#[gpui::test]
fn filter_change_clears_selection_and_new_lines_keep_it(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let logs = LogStore::new(16);
    logs.push(entry(0, Level::ERROR, "t", "first error"));
    let (pane, cx) = cx.add_window_view(|window, cx| LogsPane::new(logs.clone(), window, cx));

    pane.update(cx, |pane, cx| {
        pane.selection.select_only(0);
        pane.set_filter(LogFilter::new("", Some(Level::ERROR), vec![], vec![]), cx);
        assert_eq!(pane.selected_count(), 0);

        logs.push(entry(0, Level::WARN, "t", "warning"));
        logs.push(entry(0, Level::ERROR, "t", "second error"));
        pane.apply_rows(logs.logs());
        let messages: Vec<&str> = pane.rows.iter().map(|row| &*row.message).collect();
        assert_eq!(messages, ["first error", "second error"]);
        assert_eq!(pane.list_state.item_count(), 2);
        assert_eq!((pane.retained_count(), pane.shown_count()), (3, 2));
    });
}

#[test]
fn bounded_retention_drops_oldest() {
    let logs = LogStore::new(4);
    let layer = UiLogLayer::new(logs.clone());
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        for i in 0..10 {
            tracing::info!(iter = i, "log-line");
        }
    });
    let entries = logs.logs();
    assert_eq!(entries.len(), 4, "only the newest 4 entries should remain");
    assert!(entries.last().unwrap().message.contains("log-line"));
}

#[test]
fn follow_tail_stays_enabled_at_bottom() {
    assert!(should_follow_tail(false));
}

#[test]
fn follow_tail_pauses_when_scrolled_up() {
    assert!(!should_follow_tail(true));
}

#[test]
fn sequence_numbers_are_monotonic() {
    let logs = LogStore::new(16);
    let layer = UiLogLayer::new(logs.clone());
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..5 {
            tracing::info!("tick");
        }
    });
    let seqs: Vec<u64> = logs.logs().iter().map(|l| l.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
}

#[test]
fn compact_log_value_keeps_short_json_and_cuts_html_and_long_bodies() {
    let json = r#"HTTP error 429 with body: {"error":{"code":-32005,"message":"rate limited"}}"#;
    assert_eq!(compact_log_value(json), json);

    let html = format!(
        "HTTP error 502 with body: \n<!DOCTYPE html><html>{}</html>",
        "x".repeat(10)
    );
    assert_eq!(
        compact_log_value(&html),
        "HTTP error 502 with body: …[HTML, 38 bytes omitted]"
    );

    let long = format!("deserialization error: {}", "é".repeat(400));
    let compacted = compact_log_value(&long);
    assert!(compacted.ends_with(&format!("…[{} bytes omitted]", long.len() - 511)));
}

#[test]
fn ui_log_layer_redacts_url_credentials_paths_queries_and_fragments() {
    let logs = LogStore::new(4);
    let layer = UiLogLayer::new(logs.clone());
    let subscriber = Registry::default().with(layer);
    // Byte MAX_LOG_FIELD_LEN falls inside `secret`, the debug field's leading quote included.
    let long_str = format!("{}https://user:secret@host/", "x".repeat(496));
    let long_debug = format!("{}https://user:secret@host/", "x".repeat(495));
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(
            rpc = "https://user:pass@example.com/private/project-id?token=secret#fragment",
            proxy_url = "socks5h://proxy-user:proxy-pass@proxy.example:9050",
            long_str = long_str.as_str(),
            long_debug = ?long_debug,
            "request failed at https://api-user:api-pass@api.example/rpc/key?api_key=secret#fragment"
        );
    });

    let entry = logs.logs().pop().expect("log entry");
    assert!(entry.message.contains("https://example.com/"));
    assert!(entry.message.contains("socks5h://proxy.example:9050"));
    assert!(entry.message.contains("https://api.example/"));
    assert!(!entry.message.contains("user"));
    assert!(!entry.message.contains("pass"));
    assert!(!entry.message.contains("project-id"));
    assert!(!entry.message.contains("sec"));
    assert!(!entry.message.contains("fragment"));
}
