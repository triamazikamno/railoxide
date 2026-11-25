use super::*;

fn proposal_source_slice(
    source: &str,
    position: Option<&markdown::unist::Position>,
) -> Option<String> {
    let position = position?;
    source
        .get(position.start.offset..position.end.offset)
        .map(str::to_owned)
}

pub(super) fn inert_raw_fallback_source(source: &str) -> String {
    let fence = "`".repeat(max_backtick_run(source).saturating_add(1).max(3));
    format!("{fence}\n{source}\n{fence}")
}

pub(super) fn table_fingerprint(table: &markdown::mdast::Table) -> Option<TableFingerprint> {
    let mut row_cell_counts = Vec::with_capacity(table.children.len());
    for row in &table.children {
        let Node::TableRow(row) = row else {
            return None;
        };
        if row
            .children
            .iter()
            .any(|cell| !matches!(cell, Node::TableCell(_)))
        {
            return None;
        }
        row_cell_counts.push(row.children.len());
    }
    Some(TableFingerprint {
        alignment: table.align.clone(),
        row_cell_counts,
    })
}

fn proposal_table_render_width_px(table: &markdown::mdast::Table) -> Option<u16> {
    let mut column_lengths = Vec::new();
    for row in &table.children {
        let Node::TableRow(row) = row else {
            return None;
        };
        for (column, cell) in row.children.iter().enumerate() {
            if !matches!(cell, Node::TableCell(_)) {
                return None;
            }
            if column_lengths.len() <= column {
                column_lengths.resize(column + 1, DEFAULT_TABLE_COLUMN_LENGTH);
            }
            let length = cell.to_string().len().min(MAX_TABLE_COLUMN_LENGTH);
            column_lengths[column] = column_lengths[column]
                .max(length)
                .min(MAX_TABLE_COLUMN_LENGTH);
        }
    }

    let total_length = column_lengths
        .iter()
        .try_fold(0usize, |total, length| total.checked_add(*length))?;
    let width_per_byte = usize::from(APP_TEXT_SIZE.ceil());
    let content_width = total_length.checked_mul(width_per_byte)?;
    let column_chrome = column_lengths.len().checked_mul(TABLE_COLUMN_CHROME_PX)?;
    content_width
        .checked_add(column_chrome)?
        .checked_add(TABLE_OUTER_BORDER_PX)
        .map(|width| width.clamp(MIN_TABLE_RENDER_WIDTH_PX, MAX_TABLE_RENDER_WIDTH_PX))
        .and_then(|width| u16::try_from(width).ok())
}

fn parsed_table_fingerprint(source: &str) -> Option<TableFingerprint> {
    let root = parse_markdown(source).ok()?;
    let [Node::Table(table)] = root.children() else {
        return None;
    };
    table_fingerprint(table)
}

fn source_line_start_offset(source: &str, line: usize) -> Option<usize> {
    if line == 0 {
        return None;
    }
    if line == 1 {
        return Some(0);
    }
    let bytes = source.as_bytes();
    let mut current_line = 1;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' || (bytes[index] == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
        {
            current_line += 1;
            if current_line == line {
                return Some(index + 1);
            }
        }
        index += 1;
    }
    None
}

fn validate_table_start_position(source: &str, position: &markdown::unist::Position) -> bool {
    if position.start.column == 0
        || position.start.offset > position.end.offset
        || source
            .get(position.start.offset..position.end.offset)
            .is_none()
    {
        return false;
    }
    let Some(line_start) = source_line_start_offset(source, position.start.line) else {
        return false;
    };
    let Some(prefix) = source.get(line_start..position.start.offset) else {
        return false;
    };
    let mut column = 1usize;
    for character in prefix.chars() {
        if character == '\t' {
            let remainder = column % 4;
            column = column.saturating_add(if remainder == 0 { 1 } else { 5 - remainder });
        } else {
            column = column.saturating_add(1);
        }
    }
    column == position.start.column
}

fn prefix_bytes_to_table_column(line: &str, target_column: usize) -> Option<usize> {
    let mut visual_column = 0usize;
    let mut byte_offset = 0usize;
    while visual_column < target_column {
        let byte = *line.as_bytes().get(byte_offset)?;
        match byte {
            b' ' => {
                visual_column = visual_column.checked_add(1)?;
                byte_offset += 1;
            }
            b'\t' => {
                let tab_width = 4 - (visual_column % 4);
                visual_column = visual_column.checked_add(tab_width)?;
                if visual_column > target_column {
                    return None;
                }
                byte_offset += 1;
            }
            b'>' => {
                visual_column = visual_column.checked_add(1)?;
                if visual_column > target_column {
                    return None;
                }
                byte_offset += 1;
            }
            _ => return None,
        }
    }
    (visual_column == target_column).then_some(byte_offset)
}

fn split_table_line(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else if let Some(body) = line.strip_suffix('\r') {
        (body, "\r")
    } else {
        (line, "")
    }
}

fn normalize_table_source(
    source: &str,
    position: &markdown::unist::Position,
    raw: &str,
) -> Option<String> {
    if !validate_table_start_position(source, position) {
        return None;
    }
    let target_column = position.start.column.checked_sub(1)?;
    let mut normalized = String::with_capacity(raw.len());
    let mut first_line = true;
    let mut cursor = 0;
    while cursor < raw.len() {
        let remaining = &raw[cursor..];
        let segment_len = remaining
            .find(['\r', '\n'])
            .map_or(remaining.len(), |index| {
                if remaining.as_bytes().get(index) == Some(&b'\r')
                    && remaining.as_bytes().get(index + 1) == Some(&b'\n')
                {
                    index + 2
                } else {
                    index + 1
                }
            });
        let segment = &remaining[..segment_len];
        let (body, ending) = split_table_line(segment);
        if first_line {
            normalized.push_str(segment);
            first_line = false;
        } else {
            let prefix_len = prefix_bytes_to_table_column(body, target_column)?;
            normalized.push_str(body.get(prefix_len..)?);
            normalized.push_str(ending);
        }
        cursor += segment_len;
    }
    (!first_line).then_some(normalized)
}

fn prepare_table_source(
    source: &str,
    table: &markdown::mdast::Table,
) -> Result<(String, TableFingerprint), ProposalPreparationFailure> {
    let fingerprint =
        table_fingerprint(table).ok_or(ProposalPreparationFailure::RawParseFallback)?;
    let position = table
        .position
        .as_ref()
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if !validate_table_start_position(source, position) {
        return Err(ProposalPreparationFailure::RawParseFallback);
    }
    let raw = proposal_source_slice(source, Some(position))
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if parsed_table_fingerprint(&raw).is_some_and(|parsed| parsed == fingerprint) {
        return Ok((raw, fingerprint));
    }
    let normalized = normalize_table_source(source, position, &raw)
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if parsed_table_fingerprint(&normalized).is_some_and(|parsed| parsed == fingerprint) {
        Ok((normalized, fingerprint))
    } else {
        Err(ProposalPreparationFailure::RawParseFallback)
    }
}

fn validate_prepared_table_source(
    source: &str,
    fingerprint: &TableFingerprint,
    expected_definition_ids: &[&str],
) -> bool {
    let Ok(root) = parse_markdown(source) else {
        return false;
    };
    let Some(Node::Table(table)) = root.children().first() else {
        return false;
    };
    if table_fingerprint(table).as_ref() != Some(fingerprint)
        || root.children().len() != expected_definition_ids.len() + 1
    {
        return false;
    }
    root.children()[1..]
        .iter()
        .map(|node| match node {
            Node::Definition(definition) => Some(definition.identifier.as_str()),
            _ => None,
        })
        .eq(expected_definition_ids.iter().copied().map(Some))
}

#[cfg(test)]
pub(super) fn parse_proposal_blocks(source: &str) -> Option<Vec<ProposalBlock>> {
    match prepare_proposal_presentation(source) {
        ProposalPresentation::Prepared(prepared) => Some(prepared.blocks.clone()),
        ProposalPresentation::RawParseFallback(_) | ProposalPresentation::TooComplex => None,
    }
}

/// Rejects descriptions whose size would make parsing slow. `parse_markdown` bounds line nesting.
pub(super) const fn proposal_markdown_admitted(source: &str) -> bool {
    source.len() <= MAX_PROPOSAL_DESCRIPTION_BYTES
}

pub(super) fn prepare_proposal_presentation(source: &str) -> ProposalPresentation {
    if !proposal_markdown_admitted(source) {
        return ProposalPresentation::TooComplex;
    }
    let prepared = (|| {
        let root = parse_markdown(source).map_err(|error| match error {
            MarkdownParseError::Invalid => ProposalPreparationFailure::RawParseFallback,
            MarkdownParseError::TooDeep => ProposalPreparationFailure::TooComplex,
        })?;
        ensure_mdast_node_limit(root.children())?;
        let definitions = collect_effective_definitions(source, root.children())
            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
        let mut budget = PreparationBudget::default();
        let mut table_ordinal = 0;
        let blocks = parse_proposal_block_nodes(
            source,
            root.children(),
            &definitions,
            &mut budget,
            &mut table_ordinal,
        )?;
        Ok(PreparedProposal {
            blocks,
            table_count: table_ordinal,
        })
    })();
    match prepared {
        Ok(prepared) => ProposalPresentation::Prepared(Arc::new(prepared)),
        Err(ProposalPreparationFailure::RawParseFallback) => {
            ProposalPresentation::RawParseFallback(inert_raw_fallback_source(source))
        }
        Err(ProposalPreparationFailure::TooComplex) => ProposalPresentation::TooComplex,
    }
}

impl From<SanitizeFailure> for ProposalPreparationFailure {
    fn from(failure: SanitizeFailure) -> Self {
        match failure {
            SanitizeFailure::TooLarge => Self::TooComplex,
            SanitizeFailure::Unsupported => Self::RawParseFallback,
        }
    }
}

pub(super) fn ensure_mdast_node_limit(
    root_children: &[Node],
) -> Result<(), ProposalPreparationFailure> {
    let mut nodes = root_children.iter().collect::<Vec<_>>();
    let mut count = 1usize;
    if count > MAX_MDAST_NODES {
        return Err(ProposalPreparationFailure::TooComplex);
    }
    while let Some(node) = nodes.pop() {
        count = count
            .checked_add(1)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if count > MAX_MDAST_NODES {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        if let Some(children) = node.children() {
            nodes.extend(children);
        }
    }
    Ok(())
}

impl PreparationBudget {
    fn account_weight(&mut self, weight: usize) -> Result<(), ProposalPreparationFailure> {
        let next = self
            .weighted_complexity
            .checked_add(weight)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if next > MAX_RENDER_COMPLEXITY {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        self.weighted_complexity = next;
        Ok(())
    }

    const fn remaining_source_bytes(&self) -> usize {
        MAX_PREPARED_SOURCE_BYTES.saturating_sub(self.prepared_source_bytes)
    }

    fn ensure_markdown_fits(
        &self,
        source_len: usize,
        referenced: &[&str],
    ) -> Result<(), ProposalPreparationFailure> {
        let prepared_len = prepared_markdown_source_len(source_len, referenced)?;
        match self.prepared_source_bytes.checked_add(prepared_len) {
            Some(next) if next <= MAX_PREPARED_SOURCE_BYTES => Ok(()),
            _ => Err(ProposalPreparationFailure::TooComplex),
        }
    }

    pub(super) fn account_markdown(
        &mut self,
        source_bytes: usize,
    ) -> Result<(), ProposalPreparationFailure> {
        let next_source_bytes = self
            .prepared_source_bytes
            .checked_add(source_bytes)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if next_source_bytes > MAX_PREPARED_SOURCE_BYTES {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        self.account_weight(3)?;
        self.prepared_source_bytes = next_source_bytes;
        Ok(())
    }

    pub(super) fn account_list_item(&mut self) -> Result<(), ProposalPreparationFailure> {
        self.account_weight(1)
    }

    pub(super) fn account_blockquote(&mut self) -> Result<(), ProposalPreparationFailure> {
        self.account_weight(1)
    }
}

fn referenced_definitions<'a>(
    references: &BTreeSet<String>,
    definitions: &'a [(String, String)],
) -> Vec<&'a str> {
    definitions
        .iter()
        .filter(|(identifier, _)| references.contains(identifier))
        .map(|(_, source)| source.as_str())
        .collect()
}

pub(super) fn prepared_markdown_source_len(
    source_len: usize,
    referenced: &[&str],
) -> Result<usize, ProposalPreparationFailure> {
    if referenced.is_empty() {
        return Ok(source_len);
    }
    let appendix_len = referenced.iter().try_fold(0usize, |length, source| {
        length
            .checked_add(source.len())
            .ok_or(ProposalPreparationFailure::TooComplex)
    })?;
    let separators = referenced
        .len()
        .checked_sub(1)
        .ok_or(ProposalPreparationFailure::TooComplex)?;
    source_len
        .checked_add(2)
        .and_then(|length| length.checked_add(appendix_len))
        .and_then(|length| length.checked_add(separators))
        .ok_or(ProposalPreparationFailure::TooComplex)
}

fn collect_effective_definitions(source: &str, nodes: &[Node]) -> Option<Vec<(String, String)>> {
    let mut definitions = Vec::new();
    let mut seen = BTreeSet::new();
    collect_definition_nodes(source, nodes, &mut seen, &mut definitions)?;
    Some(definitions)
}

fn collect_definition_nodes(
    source: &str,
    nodes: &[Node],
    seen: &mut BTreeSet<String>,
    definitions: &mut Vec<(String, String)>,
) -> Option<()> {
    let mut pending = nodes.iter().rev().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            let source_slice = proposal_source_slice(source, definition.position.as_ref())?;
            if seen.insert(definition.identifier.clone()) {
                definitions.push((definition.identifier.clone(), source_slice));
            }
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().rev());
        }
    }
    Some(())
}

pub(super) fn parse_proposal_block_nodes(
    source: &str,
    nodes: &[Node],
    definitions: &[(String, String)],
    budget: &mut PreparationBudget,
    table_ordinal: &mut usize,
) -> Result<Vec<ProposalBlock>, ProposalPreparationFailure> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < nodes.len() {
        let block = match &nodes[index] {
            Node::Definition(definition) => {
                proposal_source_slice(source, definition.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                index += 1;
                continue;
            }
            Node::List(list) => {
                // Validate the list range even though its children provide the rendered text.
                proposal_source_slice(source, list.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let items = list
                    .children
                    .iter()
                    .map(|child| {
                        let Node::ListItem(item) = child else {
                            return Err(ProposalPreparationFailure::RawParseFallback);
                        };
                        proposal_source_slice(source, item.position.as_ref())
                            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                        budget.account_list_item()?;
                        Ok(ProposalListItem {
                            checked: item.checked,
                            blocks: parse_proposal_block_nodes(
                                source,
                                &item.children,
                                definitions,
                                budget,
                                table_ordinal,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                ProposalBlock::List(ProposalList {
                    ordered: list.ordered,
                    start: list.start,
                    items,
                })
            }
            Node::Blockquote(blockquote) => {
                proposal_source_slice(source, blockquote.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                budget.account_blockquote()?;
                ProposalBlock::Blockquote(ProposalBlockquote {
                    blocks: parse_proposal_block_nodes(
                        source,
                        &blockquote.children,
                        definitions,
                        budget,
                        table_ordinal,
                    )?,
                })
            }
            Node::Table(table) => {
                let (source_slice, fingerprint) = prepare_table_source(source, table)?;
                let render_width_px = proposal_table_render_width_px(table)
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let mut references = BTreeSet::new();
                collect_reference_ids(&Node::Table(table.clone()), &mut references);
                let referenced = referenced_definitions(&references, definitions);
                let expected_definition_ids = definitions
                    .iter()
                    .filter(|(identifier, _)| references.contains(identifier))
                    .map(|(identifier, _)| identifier.as_str())
                    .collect::<Vec<_>>();
                budget.ensure_markdown_fits(source_slice.len(), &referenced)?;
                let source = if referenced.is_empty() {
                    source_slice
                } else {
                    format!("{source_slice}\n\n{}", referenced.join("\n"))
                };
                if !validate_prepared_table_source(&source, &fingerprint, &expected_definition_ids)
                {
                    return Err(ProposalPreparationFailure::RawParseFallback);
                }
                let render_source =
                    sanitize_render_source(&source, budget.remaining_source_bytes())?;
                if !validate_prepared_table_source(
                    &render_source,
                    &fingerprint,
                    &expected_definition_ids,
                ) {
                    return Err(ProposalPreparationFailure::RawParseFallback);
                }
                budget.account_markdown(render_source.len())?;
                let ordinal = *table_ordinal;
                *table_ordinal = (*table_ordinal)
                    .checked_add(1)
                    .ok_or(ProposalPreparationFailure::TooComplex)?;
                ProposalBlock::Table(ProposalTable {
                    source,
                    render_source,
                    ordinal,
                    render_width_px,
                })
            }
            _ => {
                let start = nodes[index]
                    .position()
                    .and_then(|position| {
                        source
                            .get(position.start.offset..position.start.offset)
                            .map(|_| position.start.offset)
                    })
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let mut end = start;
                let mut references = BTreeSet::new();
                let mut next = index;
                while next < nodes.len()
                    && !matches!(
                        nodes[next],
                        Node::Definition(_) | Node::Table(_) | Node::List(_) | Node::Blockquote(_)
                    )
                {
                    let position = nodes[next]
                        .position()
                        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                    end = position.end.offset;
                    collect_reference_ids(&nodes[next], &mut references);
                    next += 1;
                }
                let source_slice = source
                    .get(start..end)
                    .map(str::to_owned)
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let referenced = referenced_definitions(&references, definitions);
                budget.ensure_markdown_fits(source_slice.len(), &referenced)?;
                let source_slice = if referenced.is_empty() {
                    source_slice
                } else {
                    format!("{source_slice}\n\n{}", referenced.join("\n"))
                };
                let source_slice =
                    sanitize_render_source(&source_slice, budget.remaining_source_bytes())?;
                budget.account_markdown(source_slice.len())?;
                index = next;
                ProposalBlock::Markdown(source_slice)
            }
        };
        if matches!(
            &block,
            ProposalBlock::Table(_) | ProposalBlock::List(_) | ProposalBlock::Blockquote(_)
        ) {
            index += 1;
        }
        blocks.push(block);
    }
    Ok(blocks)
}

fn collect_reference_ids(node: &Node, references: &mut BTreeSet<String>) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node {
            Node::LinkReference(reference) => {
                references.insert(reference.identifier.clone());
            }
            Node::ImageReference(reference) => {
                references.insert(reference.identifier.clone());
            }
            _ => {}
        }
        if let Some(children) = node.children() {
            pending.extend(children);
        }
    }
}
