use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    Bounds, Render, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, point, size,
};

use super::*;
use gpui_component::scroll::ScrollbarHandle;

struct ParticipantPickerProbe {
    governance: GovernanceState,
    accounts: Vec<PublicAccountMetadata>,
    saved: Vec<String>,
    commits: Vec<Vec<String>>,
}

impl ParticipantPickerProbe {
    fn sync_picker(&mut self, window: &mut Window, cx: &mut App) {
        let selected = self.saved.iter().cloned().collect();
        let items = participant_choices(&self.accounts, &selected, Some("wallet"), "");
        self.governance.sync_participant_picker(
            Some("wallet".to_owned()),
            items,
            &self.saved,
            window,
            cx,
        );
    }
}

impl Render for ParticipantPickerProbe {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.sync_picker(window, cx);
        div()
            .p_3()
            .child(Combobox::new(&self.governance.participant_picker).w(px(220.0)))
    }
}

struct TableProbeDelegate {
    columns: Vec<(StakeTableColumnKind, Column)>,
    compact: bool,
    row_count: usize,
    action_clicked: Option<Arc<AtomicBool>>,
}

impl TableProbeDelegate {
    fn new() -> Self {
        Self::with_rows(5)
    }

    fn with_rows(row_count: usize) -> Self {
        let layout = staking_table_layout(STAKING_TABLE_MIN_WIDTH);
        Self {
            compact: layout.compact,
            columns: layout.columns,
            row_count,
            action_clicked: None,
        }
    }

    fn with_action_callback(action_clicked: Arc<AtomicBool>) -> Self {
        let mut delegate = Self::new();
        delegate.action_clicked = Some(action_clicked);
        delegate
    }
}

impl TableDelegate for TableProbeDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.row_count
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].1.clone()
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("header")
            .absolute()
            .h(px(0.0))
            .max_h(px(0.0))
            .border_0()
            .overflow_hidden()
    }

    fn render_th(
        &mut self,
        _col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        div().h(px(0.0)).max_h(px(0.0)).overflow_hidden()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        let row = render_staking_table_row(row_ix);
        if row_ix == 5 {
            row.debug_selector(|| "staking-test-row-six".to_owned())
        } else if row_ix == 0 {
            row.debug_selector(|| "staking-test-row".to_owned())
        } else {
            row
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        if self.columns[col_ix].0 == StakeTableColumnKind::Stake {
            div()
                .debug_selector(|| "staking-stake-content".to_owned())
                .h_full()
                .flex()
                .items_center()
                .child(
                    Popover::new(SharedString::from(format!(
                        "staking-test-stake-popover-{row_ix}"
                    )))
                    .trigger(
                        Button::new(SharedString::from(format!(
                            "staking-test-stake-trigger-{row_ix}"
                        )))
                        .debug_selector(|| "staking-test-stake-trigger".to_owned())
                        .text()
                        .xsmall()
                        .child("#123…90"),
                    )
                    .content(|_state, _window, _cx| div().child("stake details")),
                )
        } else if self.columns[col_ix].0 == StakeTableColumnKind::Actions {
            div()
                .debug_selector(|| "staking-actions-content".to_owned())
                .h_full()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .child(
                    app_button_base(SharedString::from(format!(
                        "staking-test-undelegate-{row_ix}"
                    )))
                    .debug_selector(|| "staking-test-undelegate".to_owned())
                    .small()
                    .outline()
                    .child("Undelegate")
                    .on_click({
                        let action_clicked = self.action_clicked.clone();
                        move |_event, _window, _cx| {
                            if let Some(action_clicked) = &action_clicked {
                                action_clicked.store(true, Ordering::SeqCst);
                            }
                        }
                    }),
                )
                .child(
                    app_button_base(SharedString::from(format!("staking-test-unlock-{row_ix}")))
                        .debug_selector(|| "staking-test-unlock".to_owned())
                        .small()
                        .outline()
                        .child(if self.compact {
                            "Unlock"
                        } else {
                            "Unlock · 2 steps"
                        }),
                )
        } else if self.columns[col_ix].0 == StakeTableColumnKind::State {
            div()
                .debug_selector(|| "staking-state-content".to_owned())
                .h_full()
                .flex()
                .items_center()
                .child(
                    div()
                        .debug_selector(|| "staking-state-label".to_owned())
                        .child(app_status_tag("Unlocking", theme::WARNING)),
                )
        } else {
            div().size_full()
        }
    }
}

struct TableRenderProbe {
    table: Entity<TableState<TableProbeDelegate>>,
    table_height: gpui::Pixels,
    external_scrollbar: bool,
}

impl Render for TableRenderProbe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let vertical_scroll_handle = self.table.read(cx).vertical_scroll_handle.clone();
        let viewport = div()
            .debug_selector(|| "staking-visible-probe-viewport".to_owned())
            .w_full()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .child(
                DataTable::new(&self.table)
                    .large()
                    .bordered(false)
                    .scrollbar_visible(false, false),
            );
        div()
            .relative()
            .w_full()
            .h(self.table_height)
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(viewport)
            .when(self.external_scrollbar, |this| {
                this.child(
                    div()
                        .occlude()
                        .debug_selector(|| "staking-test-external-scrollbar".to_owned())
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .w(STAKING_TABLE_SCROLLBAR_WIDTH)
                        .child(Scrollbar::vertical(&vertical_scroll_handle)),
                )
            })
    }
}

struct AccountHeaderProbe {
    compact: bool,
    width: gpui::Pixels,
    height: gpui::Pixels,
    prefix: &'static str,
}

impl Render for AccountHeaderProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let identity = div()
            .flex()
            .items_center()
            .child(app_strong_text("Imported account"));
        div()
            .debug_selector(|| "staking-account-header-card".to_owned())
            .w(self.width)
            .h(self.height)
            .p_3()
            .child(render_staking_account_header(
                identity,
                voting_power_metric("123456789012345678901234 RAIL", false, None),
                self.compact,
                Some(self.prefix),
            ))
    }
}

fn account(uuid: &str, status: PublicAccountStatus, global: bool) -> PublicAccountMetadata {
    PublicAccountMetadata {
        public_account_uuid: uuid.to_owned(),
        address: Address::from([uuid.as_bytes()[0]; 20]),
        label: Some(uuid.to_owned()),
        source: wallet_ops::vault::PublicAccountSource::Imported,
        scope: if global {
            wallet_ops::vault::PublicAccountScope::Global
        } else {
            wallet_ops::vault::PublicAccountScope::PrivateWallet {
                wallet_uuid: "wallet".to_owned(),
            }
        },
        derivation_index: None,
        hardware_descriptor: None,
        status,
        display_order: 0,
    }
}

fn key(wallet: &str, chain: u64, uuid: &str, address: Address) -> GovernanceContextKey {
    GovernanceContextKey {
        wallet_id: Some(wallet.to_owned()),
        chain_id: chain,
        participants: vec![GovernanceParticipantIdentity {
            uuid: uuid.to_owned(),
            address,
        }],
    }
}

alloy::sol! {
    interface GovernanceRpcFixture {
        function stakingToken() external view returns (address);
        function staking() external view returns (address);
        function proposalsLength() external view returns (uint256);
        function SPONSOR_WINDOW() external view returns (uint256);
        function VOTING_START_OFFSET() external view returns (uint256);
        function VOTING_YAY_END_OFFSET() external view returns (uint256);
        function VOTING_NAY_END_OFFSET() external view returns (uint256);
        function EXECUTION_START_OFFSET() external view returns (uint256);
        function EXECUTION_END_OFFSET() external view returns (uint256);
        function proposals(uint256 index) external view returns (
            bool executed,
            address proposer,
            string proposalDocument,
            uint256 publishTime,
            uint256 voteCallTime,
            uint256 sponsorship,
            uint256 yayVotes,
            uint256 nayVotes,
            uint256 sponsorInterval,
            uint256 votingInterval
        );
        function getActions(uint256 id) external view returns (tuple(address, bytes, uint256)[]);
        function accountSnapshotLength(address owner) external view returns (uint256);
        function accountSnapshotAt(address owner, uint256 index, uint256 hint)
            external view returns (uint256 interval, uint256 votingPower);
        function lastSponsored(address account)
            external view returns (uint256 lastSponsorTime, uint256 proposalID);
        function getSponsored(uint256 proposalID, address account)
            external view returns (uint256);
        function getClaimed(address account, address token, uint256 interval)
            external view returns (bool);
        function calculateRewards(
            address[] tokens,
            address account,
            uint256 startingInterval,
            uint256 endingInterval,
            uint256[] hints,
            bool ignoreClaimed
        ) external view returns (uint256[]);
    }
}

/// One JSON-RPC call seen by the governance RPC mock.
#[derive(Debug)]
struct SeenRpcCall {
    endpoint: &'static str,
    client: String,
    method: String,
}

/// Chain state the governance RPC mock serves, which a test may change while it runs.
struct GovernanceMockChain {
    /// Proposals on the voting contract.
    proposals: std::sync::atomic::AtomicU64,
    voting_interval: std::sync::atomic::AtomicU64,
    /// Requests tagged with this client wait until it is cleared.
    held_client: std::sync::Mutex<Option<String>>,
    released: std::sync::Condvar,
}

/// A chain whose only General endpoint and only Sensitive endpoint are separate mocks, with a
/// shared wallet client and an action's client that tag their requests.
struct GovernanceRpcMock {
    seen: Arc<std::sync::Mutex<Vec<SeenRpcCall>>>,
    chain: Arc<GovernanceMockChain>,
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    http: wallet_ops::HttpContext,
    client: wallet_ops::OperationHttpClient,
    isolated: wallet_ops::ClassRoute,
}

impl GovernanceRpcMock {
    fn take(&self) -> Vec<SeenRpcCall> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }

    fn hold(&self, client: Option<String>) {
        *self.chain.held_client.lock().unwrap() = client;
        self.chain.released.notify_all();
    }
}

mod dashboard_state;
mod governance_rpc;
mod participant_reads;
mod reward_intervals;
mod rewards;
mod staking_table;
