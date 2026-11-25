use super::*;

pub(in crate::root) async fn load_initial(
    chain_id: u64,
    requested_page: usize,
    route: &ClassRoute,
    http: &HttpContext,
) -> eyre::Result<InitialLoad> {
    let overview = fetch_governance_overview(chain_id, route, http).await?;
    let Some(overview) = overview else {
        return Ok(InitialLoad {
            overview: None,
            chain_time_anchor: None,
            total_pages: 0,
            page: 0,
            rows: Vec::new(),
        });
    };
    let chain_time = fetch_governance_chain_time(route, http).await?;
    let chain_time_anchor = ChainTimeAnchor {
        chain_time,
        captured_at: Instant::now(),
    };
    let total = total_proposals(&overview)?;
    let total_pages = total.div_ceil(PROPOSALS_PAGE_SIZE);
    let page = if total_pages == 0 {
        0
    } else {
        requested_page.min(total_pages - 1)
    };
    let rows = if total == 0 {
        Vec::new()
    } else {
        load_page(&overview, page, route, http).await?
    };
    Ok(InitialLoad {
        overview: Some(overview),
        chain_time_anchor: Some(chain_time_anchor),
        total_pages,
        page,
        rows,
    })
}
pub(super) async fn load_page(
    overview: &GovernanceOverview,
    page: usize,
    route: &ClassRoute,
    http: &HttpContext,
) -> eyre::Result<Vec<ResolvedProposal>> {
    let size = std::num::NonZeroUsize::new(PROPOSALS_PAGE_SIZE).expect("non-zero page size");
    let proposals = fetch_governance_page(overview, page, size, route, http).await?;
    let mut rows = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        let rules = rules_for(overview, proposal.contract_version)?;
        rows.push(ResolvedProposal {
            proposal,
            rules,
            document: ProposalDocumentState::Pending,
        });
    }
    Ok(rows)
}

pub(super) async fn hydrate_page(
    rows: Vec<ResolvedProposal>,
    store: Option<&Arc<DesktopVaultStore>>,
    http: &HttpContext,
    semaphore: Arc<Semaphore>,
    completion_tx: mpsc::UnboundedSender<DocumentCompletion>,
) {
    let Some(db) = store.map(|store| store.db()) else {
        for row in rows {
            let _ = completion_tx.send(DocumentCompletion {
                identity: row.identity(),
                document: unavailable_document(),
                presentation: ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                    &unavailable_document().description,
                )),
            });
        }
        return;
    };
    let gateways = load_wallet_settings(db.as_ref())
        .ok()
        .map(|settings| settings.poi.artifact.gateway_urls)
        .unwrap_or_default();
    let futures = FuturesUnordered::new();
    for row in rows {
        let identity = row.identity();
        let cid = row.proposal.proposal_document.clone();
        let db = Arc::clone(&db);
        let http = http.clone();
        let gateways = gateways.clone();
        let semaphore = Arc::clone(&semaphore);
        futures.push(async move {
            let document = match semaphore.acquire_owned().await {
                Ok(permit) => {
                    let document = resolve_governance_document(&db, &http, &cid, &gateways).await;
                    drop(permit);
                    document
                }
                Err(_) => unavailable_document(),
            };
            let presentation = if document.available {
                let source = document.description.clone();
                let fallback_source = source.clone();
                match tokio::task::spawn_blocking(move || prepare_proposal_presentation(&source))
                    .await
                {
                    Ok(presentation) => presentation,
                    // A panic can only come from parsing, which follows admission, so this
                    // source is within the description cap.
                    Err(_) => ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                        &fallback_source,
                    )),
                }
            } else {
                ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                    &document.description,
                ))
            };
            DocumentCompletion {
                identity,
                document,
                presentation,
            }
        });
    }
    send_document_completions(futures, completion_tx).await;
}

pub(super) async fn send_document_completions<F>(
    mut futures: FuturesUnordered<F>,
    completion_tx: mpsc::UnboundedSender<DocumentCompletion>,
) where
    F: Future<Output = DocumentCompletion>,
{
    while let Some(completion) = futures.next().await {
        let _ = completion_tx.send(completion);
    }
}
fn unavailable_document() -> GovernanceDocument {
    GovernanceDocument {
        title: "Document unavailable".to_string(),
        description: String::new(),
        available: false,
    }
}
pub(super) fn total_proposals(overview: &GovernanceOverview) -> eyre::Result<usize> {
    let v2 = usize::try_from(overview.v2.proposal_count)
        .map_err(|_| eyre::eyre!("governance V2 proposal count is too large"))?;
    let v1 = overview
        .v1
        .as_ref()
        .map_or(Ok(0), |s| usize::try_from(s.proposal_count))
        .map_err(|_| eyre::eyre!("governance V1 proposal count is too large"))?;
    v2.checked_add(v1)
        .ok_or_else(|| eyre::eyre!("governance proposal count overflows platform limits"))
}
fn rules_for(
    overview: &GovernanceOverview,
    version: wallet_ops::GovernanceContractVersion,
) -> eyre::Result<GovernanceContractRules> {
    Ok(match version {
        wallet_ops::GovernanceContractVersion::V2 => overview.v2.rules.clone(),
        wallet_ops::GovernanceContractVersion::V1 => overview
            .v1
            .as_ref()
            .ok_or_else(|| eyre::eyre!("governance V1 rules are unavailable"))?
            .rules
            .clone(),
    })
}
