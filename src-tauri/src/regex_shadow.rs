use regex::{Captures, Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const REGEX_IR_NEST_LIMIT: usize = 29;
// regex-syntax also counts concat, alternation, bracketed class, and class union nodes.
const REGEX_COMPILE_NEST_LIMIT: u32 = REGEX_IR_NEST_LIMIT as u32 * 3 + 4;
const REGEX_PENDING_CANCELLATION_TTL: Duration = Duration::from_secs(2);
const MAX_PENDING_REGEX_CANCELLATIONS: usize = 64;
const MAX_REGEX_REQUEST_ID_BYTES: usize = 64;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegexShadowPlan {
    version: u8,
    entries: Vec<RegexShadowEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegexShadowEntry {
    source_index: usize,
    global: bool,
    capture_count: usize,
    pattern_bytes: usize,
    replacement_bytes: usize,
    pattern: RegexShadowPattern,
    replacement: Vec<ReplacementToken>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RegexShadowPattern {
    alternatives: Vec<RegexShadowAlternative>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RegexShadowAlternative {
    atoms: Vec<RegexShadowAtom>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RegexShadowAtom {
    Literal {
        value: u8,
    },
    Class {
        ranges: Vec<RegexShadowRange>,
    },
    Group {
        alternatives: Vec<RegexShadowAlternative>,
    },
    Capture {
        index: usize,
        alternatives: Vec<RegexShadowAlternative>,
    },
    Repeat {
        min: usize,
        max: usize,
        atom: Box<RegexShadowAtom>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct RegexShadowRange {
    start: u8,
    end: u8,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReplacementToken {
    Literal { value: String },
    Match,
    Prefix,
    Suffix,
    Capture { index: usize },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegexShadowRuleError {
    source_index: usize,
    category: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct RegexShadowResult {
    data: String,
    errors: Vec<RegexShadowRuleError>,
}

#[cfg(test)]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegexShadowEvidence {
    fixture_id: String,
    plan_fingerprint: String,
    input_hash: String,
    authority_output_hash: String,
    rust_output_hash: Option<String>,
    first_differing_utf16_index: Option<usize>,
    authority_error_source_indexes: Vec<usize>,
    rust_error_source_indexes: Vec<usize>,
    category: &'static str,
}

#[derive(Debug)]
struct RegexShadowFailure(&'static str);

#[derive(Default)]
pub(crate) struct RegexCancellationRegistry {
    requests: Arc<Mutex<HashMap<String, RegexCancellationEntry>>>,
}

enum RegexCancellationEntry {
    Pending { expires_at: Instant },
    Active(Arc<AtomicBool>),
}

struct RegexCancellationRegistration {
    request_id: String,
    cancelled: Arc<AtomicBool>,
    requests: Arc<Mutex<HashMap<String, RegexCancellationEntry>>>,
}

impl RegexCancellationRegistry {
    fn register(&self, request_id: &str) -> Result<RegexCancellationRegistration, String> {
        self.register_at(request_id, Instant::now())
    }

    fn register_at(
        &self,
        request_id: &str,
        now: Instant,
    ) -> Result<RegexCancellationRegistration, String> {
        validate_request_id(request_id)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| "regex_shadow_registry".to_string())?;
        prune_pending_cancellations(&mut requests, now);
        match requests.get(request_id) {
            Some(RegexCancellationEntry::Active(_)) => {
                return Err("regex_shadow_request_active".to_string());
            }
            Some(RegexCancellationEntry::Pending { .. }) => {
                cancelled.store(true, Ordering::Relaxed);
            }
            None => {}
        }
        requests.insert(
            request_id.to_string(),
            RegexCancellationEntry::Active(Arc::clone(&cancelled)),
        );
        Ok(RegexCancellationRegistration {
            request_id: request_id.to_string(),
            cancelled,
            requests: Arc::clone(&self.requests),
        })
    }

    fn cancel(&self, request_id: &str) -> Result<bool, String> {
        self.cancel_at(request_id, Instant::now())
    }

    fn cancel_at(&self, request_id: &str, now: Instant) -> Result<bool, String> {
        validate_request_id(request_id)?;
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| "regex_shadow_registry".to_string())?;
        prune_pending_cancellations(&mut requests, now);
        match requests.get(request_id) {
            Some(RegexCancellationEntry::Active(cancelled)) => {
                cancelled.store(true, Ordering::Relaxed);
                Ok(true)
            }
            Some(RegexCancellationEntry::Pending { .. }) => Ok(false),
            None => {
                let pending_count = requests
                    .values()
                    .filter(|entry| matches!(entry, RegexCancellationEntry::Pending { .. }))
                    .count();
                if pending_count >= MAX_PENDING_REGEX_CANCELLATIONS {
                    return Err("regex_shadow_registry_limit".to_string());
                }
                requests.insert(
                    request_id.to_string(),
                    RegexCancellationEntry::Pending {
                        expires_at: now + REGEX_PENDING_CANCELLATION_TTL,
                    },
                );
                Ok(false)
            }
        }
    }
}

fn validate_request_id(request_id: &str) -> Result<(), String> {
    if request_id.is_empty() || request_id.len() > MAX_REGEX_REQUEST_ID_BYTES {
        Err("regex_shadow_request_id".to_string())
    } else {
        Ok(())
    }
}

fn prune_pending_cancellations(
    requests: &mut HashMap<String, RegexCancellationEntry>,
    now: Instant,
) {
    requests.retain(|_, entry| {
        !matches!(entry, RegexCancellationEntry::Pending { expires_at } if *expires_at <= now)
    });
}

impl Drop for RegexCancellationRegistration {
    fn drop(&mut self) {
        let Ok(mut requests) = self.requests.lock() else {
            return;
        };
        if requests
            .get(&self.request_id)
            .is_some_and(|entry| {
                matches!(entry, RegexCancellationEntry::Active(cancelled) if Arc::ptr_eq(cancelled, &self.cancelled))
            })
        {
            requests.remove(&self.request_id);
        }
    }
}

#[derive(Clone, Copy)]
struct ExecutionControl<'a> {
    cancelled: Option<&'a AtomicBool>,
    deadline: Option<Instant>,
    after_rule_compiled: Option<&'a dyn Fn(usize)>,
}

#[derive(Clone, Copy)]
enum ExecutionPhase {
    Compile,
    Execute,
}

struct CompiledRegexShadowPlan {
    entries: Vec<CompiledRegexShadowEntry>,
}

struct CompiledRegexShadowEntry {
    source_index: usize,
    global: bool,
    replacement: Vec<ReplacementToken>,
    regex: Option<Regex>,
}

fn validate_alternatives(
    alternatives: &[RegexShadowAlternative],
    next_capture: &mut usize,
    inside_quantifier: bool,
    depth: usize,
) -> Result<usize, RegexShadowFailure> {
    if alternatives.is_empty() {
        return Err(RegexShadowFailure("regex_shadow_ir"));
    }
    let mut source_bytes = alternatives.len() - 1;
    for alternative in alternatives {
        if alternative.atoms.is_empty() {
            return Err(RegexShadowFailure("regex_shadow_ir"));
        }
        let mut nullable = true;
        for atom in &alternative.atoms {
            let (atom_nullable, atom_bytes) =
                validate_atom(atom, next_capture, inside_quantifier, depth)?;
            nullable &= atom_nullable;
            source_bytes = source_bytes.saturating_add(atom_bytes);
        }
        if nullable {
            return Err(RegexShadowFailure("regex_shadow_ir"));
        }
    }
    Ok(source_bytes)
}

fn validate_atom(
    atom: &RegexShadowAtom,
    next_capture: &mut usize,
    inside_quantifier: bool,
    depth: usize,
) -> Result<(bool, usize), RegexShadowFailure> {
    match atom {
        RegexShadowAtom::Literal { value } => {
            if *value > 0x7f {
                return Err(RegexShadowFailure("regex_shadow_ir"));
            }
            Ok((false, 1))
        }
        RegexShadowAtom::Class { ranges } => {
            if ranges.is_empty()
                || ranges
                    .iter()
                    .any(|range| range.start > range.end || range.end > 0x7f)
            {
                return Err(RegexShadowFailure("regex_shadow_ir"));
            }
            Ok((false, ranges.len().saturating_add(2)))
        }
        RegexShadowAtom::Group { alternatives } => {
            if depth >= REGEX_IR_NEST_LIMIT {
                return Err(RegexShadowFailure("regex_shadow_nest_limit"));
            }
            let bytes =
                validate_alternatives(alternatives, next_capture, inside_quantifier, depth + 1)?;
            Ok((false, bytes.saturating_add(4)))
        }
        RegexShadowAtom::Capture {
            index,
            alternatives,
        } => {
            if depth >= REGEX_IR_NEST_LIMIT {
                return Err(RegexShadowFailure("regex_shadow_nest_limit"));
            }
            if inside_quantifier {
                return Err(RegexShadowFailure("regex_shadow_capture_under_repeat"));
            }
            *next_capture += 1;
            if *index != *next_capture {
                return Err(RegexShadowFailure("regex_shadow_ir"));
            }
            let bytes =
                validate_alternatives(alternatives, next_capture, inside_quantifier, depth + 1)?;
            Ok((false, bytes.saturating_add(2)))
        }
        RegexShadowAtom::Repeat { min, max, atom } => {
            if depth >= REGEX_IR_NEST_LIMIT {
                return Err(RegexShadowFailure("regex_shadow_nest_limit"));
            }
            if inside_quantifier || min > max || *max > 64 {
                return Err(RegexShadowFailure("regex_shadow_ir"));
            }
            let (atom_nullable, bytes) = validate_atom(atom, next_capture, true, depth + 1)?;
            let quantifier_bytes = if *min == 0 && *max == 1 {
                1
            } else if min == max {
                min.to_string().len() + 2
            } else {
                min.to_string().len() + max.to_string().len() + 3
            };
            Ok((
                *min == 0 || atom_nullable,
                bytes.saturating_add(quantifier_bytes),
            ))
        }
    }
}

fn validate_entry(entry: &RegexShadowEntry) -> Result<(), RegexShadowFailure> {
    let mut capture_count = 0usize;
    let minimum_pattern_bytes =
        validate_alternatives(&entry.pattern.alternatives, &mut capture_count, false, 0)?;
    if capture_count != entry.capture_count || minimum_pattern_bytes > entry.pattern_bytes {
        return Err(RegexShadowFailure("regex_shadow_ir"));
    }
    let mut minimum_replacement_bytes = 0usize;
    for token in &entry.replacement {
        minimum_replacement_bytes = minimum_replacement_bytes.saturating_add(match token {
            ReplacementToken::Literal { value } => value.len(),
            ReplacementToken::Match | ReplacementToken::Prefix | ReplacementToken::Suffix => 2,
            ReplacementToken::Capture { index } => {
                if *index == 0 || *index > entry.capture_count {
                    return Err(RegexShadowFailure("regex_shadow_ir"));
                }
                if *index >= 10 {
                    3
                } else {
                    2
                }
            }
        });
    }
    if minimum_replacement_bytes > entry.replacement_bytes {
        return Err(RegexShadowFailure("regex_shadow_ir"));
    }
    Ok(())
}

fn check_control(
    control: ExecutionControl<'_>,
    phase: ExecutionPhase,
) -> Result<(), RegexShadowFailure> {
    if control
        .cancelled
        .is_some_and(|cancelled| cancelled.load(Ordering::Relaxed))
    {
        return Err(RegexShadowFailure(match phase {
            ExecutionPhase::Compile => "regex_shadow_cancelled_compile",
            ExecutionPhase::Execute => "regex_shadow_cancelled_execute",
        }));
    }
    if control
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err(RegexShadowFailure(match phase {
            ExecutionPhase::Compile => "regex_shadow_deadline_compile",
            ExecutionPhase::Execute => "regex_shadow_deadline_execute",
        }));
    }
    Ok(())
}

#[cfg(test)]
fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[cfg(test)]
fn first_differing_utf16_index(left: &str, right: &str) -> Option<usize> {
    let mut left_units = left.encode_utf16();
    let mut right_units = right.encode_utf16();
    let mut index = 0usize;
    loop {
        match (left_units.next(), right_units.next()) {
            (Some(left), Some(right)) if left == right => index += 1,
            (None, None) => return None,
            _ => return Some(index),
        }
    }
}

#[cfg(test)]
fn compare_shadow(
    fixture_id: &str,
    plan_json: &str,
    input: &str,
    authority_output: &str,
    authority_error_source_indexes: &[usize],
) -> RegexShadowEvidence {
    let plan_fingerprint = sha256_hex(plan_json.as_bytes());
    let input_hash = sha256_hex(input.as_bytes());
    let authority_output_hash = sha256_hex(authority_output.as_bytes());
    let result = serde_json::from_str(plan_json)
        .map_err(|_| RegexShadowFailure("regex_shadow_plan_json"))
        .and_then(|plan| {
            execute_plan_with_control(
                plan,
                input,
                ExecutionControl {
                    cancelled: None,
                    deadline: Some(Instant::now() + Duration::from_secs(2)),
                    after_rule_compiled: None,
                },
            )
        });
    match result {
        Ok(result) => {
            let rust_error_source_indexes = result
                .errors
                .iter()
                .map(|error| error.source_index)
                .collect::<Vec<_>>();
            let first_difference = first_differing_utf16_index(authority_output, &result.data);
            let category = if first_difference.is_some() {
                "regex_shadow_output_mismatch"
            } else if rust_error_source_indexes != authority_error_source_indexes {
                "regex_shadow_error_order_mismatch"
            } else {
                "match"
            };
            RegexShadowEvidence {
                fixture_id: fixture_id.to_string(),
                plan_fingerprint,
                input_hash,
                authority_output_hash,
                rust_output_hash: Some(sha256_hex(result.data.as_bytes())),
                first_differing_utf16_index: first_difference,
                authority_error_source_indexes: authority_error_source_indexes.to_vec(),
                rust_error_source_indexes,
                category,
            }
        }
        Err(error) => RegexShadowEvidence {
            fixture_id: fixture_id.to_string(),
            plan_fingerprint,
            input_hash,
            authority_output_hash,
            rust_output_hash: None,
            first_differing_utf16_index: None,
            authority_error_source_indexes: authority_error_source_indexes.to_vec(),
            rust_error_source_indexes: Vec::new(),
            category: error.0,
        },
    }
}

fn build_alternatives(alternatives: &[RegexShadowAlternative]) -> String {
    alternatives
        .iter()
        .map(|alternative| alternative.atoms.iter().map(build_atom).collect::<String>())
        .collect::<Vec<_>>()
        .join("|")
}

fn build_atom(atom: &RegexShadowAtom) -> String {
    match atom {
        RegexShadowAtom::Literal { value } => format!(r"\x{value:02X}"),
        RegexShadowAtom::Class { ranges } => {
            let mut pattern = String::from("[");
            for range in ranges {
                pattern.push_str(&format!(r"\x{:02X}", range.start));
                if range.start != range.end {
                    pattern.push('-');
                    pattern.push_str(&format!(r"\x{:02X}", range.end));
                }
            }
            pattern.push(']');
            pattern
        }
        RegexShadowAtom::Group { alternatives } => {
            format!("(?:{})", build_alternatives(alternatives))
        }
        RegexShadowAtom::Capture {
            index: _,
            alternatives,
        } => format!("({})", build_alternatives(alternatives)),
        RegexShadowAtom::Repeat { min, max, atom } => {
            format!("{}{{{min},{max}}}", build_atom(atom))
        }
    }
}

fn append_substitution(
    output: &mut String,
    tokens: &[ReplacementToken],
    input: &str,
    captures: &Captures<'_>,
    output_limit: usize,
) -> Result<(), RegexShadowFailure> {
    let full_match = captures.get(0).expect("capture zero must exist");
    for token in tokens {
        match token {
            ReplacementToken::Literal { value } => push_limited(output, value, output_limit)?,
            ReplacementToken::Match => push_limited(output, full_match.as_str(), output_limit)?,
            ReplacementToken::Prefix => {
                push_limited(output, &input[..full_match.start()], output_limit)?
            }
            ReplacementToken::Suffix => {
                push_limited(output, &input[full_match.end()..], output_limit)?
            }
            ReplacementToken::Capture { index } => {
                if let Some(capture) = captures.get(*index) {
                    push_limited(output, capture.as_str(), output_limit)?;
                }
            }
        }
    }
    Ok(())
}

fn push_limited(
    output: &mut String,
    value: &str,
    output_limit: usize,
) -> Result<(), RegexShadowFailure> {
    if output.len().saturating_add(value.len()) > output_limit {
        return Err(RegexShadowFailure("regex_shadow_output_limit"));
    }
    output.push_str(value);
    Ok(())
}

#[cfg(test)]
fn execute_plan(
    plan: RegexShadowPlan,
    input: &str,
) -> Result<RegexShadowResult, RegexShadowFailure> {
    execute_plan_with_control(
        plan,
        input,
        ExecutionControl {
            cancelled: None,
            deadline: None,
            after_rule_compiled: None,
        },
    )
}

fn execute_batch(
    plan: RegexShadowPlan,
    input: String,
    cancelled: Option<&AtomicBool>,
) -> Result<RegexShadowResult, RegexShadowFailure> {
    execute_plan_with_control(
        plan,
        &input,
        ExecutionControl {
            cancelled,
            deadline: Some(Instant::now() + Duration::from_secs(2)),
            after_rule_compiled: None,
        },
    )
}

#[tauri::command(async)]
pub(crate) async fn regex_execute_batch(
    request_id: String,
    plan: RegexShadowPlan,
    input: String,
    registry: tauri::State<'_, RegexCancellationRegistry>,
) -> Result<RegexShadowResult, String> {
    let execution = registry.register(&request_id)?;
    let cancelled = Arc::clone(&execution.cancelled);
    let result =
        tauri::async_runtime::spawn_blocking(move || execute_batch(plan, input, Some(&cancelled)))
            .await
            .map_err(|_| "regex_shadow_join".to_string())?
            .map_err(|error| error.0.to_string());
    drop(execution);
    result
}

#[tauri::command]
pub(crate) fn regex_cancel_batch(
    request_id: String,
    registry: tauri::State<'_, RegexCancellationRegistry>,
) -> Result<bool, String> {
    registry.cancel(&request_id)
}

fn execute_plan_with_control(
    plan: RegexShadowPlan,
    input: &str,
    control: ExecutionControl<'_>,
) -> Result<RegexShadowResult, RegexShadowFailure> {
    let plan = compile_plan_with_control(plan, control)?;
    execute_compiled_plan(&plan, input, control)
}

#[cfg(test)]
fn compile_plan(plan: RegexShadowPlan) -> Result<CompiledRegexShadowPlan, RegexShadowFailure> {
    compile_plan_with_control(
        plan,
        ExecutionControl {
            cancelled: None,
            deadline: None,
            after_rule_compiled: None,
        },
    )
}

fn compile_plan_with_control(
    plan: RegexShadowPlan,
    control: ExecutionControl<'_>,
) -> Result<CompiledRegexShadowPlan, RegexShadowFailure> {
    check_control(control, ExecutionPhase::Compile)?;
    if plan.version != 1 {
        return Err(RegexShadowFailure("regex_shadow_version"));
    }
    if plan.entries.is_empty() || plan.entries.len() > 500 {
        return Err(RegexShadowFailure("regex_shadow_rule_limit"));
    }
    let mut pattern_bytes = 0usize;
    let mut replacement_bytes = 0usize;
    for entry in &plan.entries {
        check_control(control, ExecutionPhase::Compile)?;
        if entry.pattern_bytes > 4_096 {
            return Err(RegexShadowFailure("regex_shadow_pattern_limit"));
        }
        if entry.capture_count > 99 {
            return Err(RegexShadowFailure("regex_shadow_capture_limit"));
        }
        validate_entry(entry)?;
        pattern_bytes = pattern_bytes.saturating_add(entry.pattern_bytes);
        replacement_bytes = replacement_bytes.saturating_add(entry.replacement_bytes);
    }
    if pattern_bytes > 65_536 {
        return Err(RegexShadowFailure("regex_shadow_pattern_total_limit"));
    }
    if replacement_bytes > 65_536 {
        return Err(RegexShadowFailure("regex_shadow_replacement_total_limit"));
    }
    let mut entries = Vec::with_capacity(plan.entries.len());
    for (index, entry) in plan.entries.into_iter().enumerate() {
        check_control(control, ExecutionPhase::Compile)?;
        let pattern = build_alternatives(&entry.pattern.alternatives);
        let regex = RegexBuilder::new(&pattern)
            .nest_limit(REGEX_COMPILE_NEST_LIMIT)
            .build()
            .ok();
        if let Some(after_rule_compiled) = control.after_rule_compiled {
            after_rule_compiled(index);
        }
        check_control(control, ExecutionPhase::Compile)?;
        if regex
            .as_ref()
            .is_some_and(|regex| regex.captures_len() != entry.capture_count + 1)
        {
            return Err(RegexShadowFailure("regex_shadow_ir"));
        }
        entries.push(CompiledRegexShadowEntry {
            source_index: entry.source_index,
            global: entry.global,
            replacement: entry.replacement,
            regex,
        });
    }
    check_control(control, ExecutionPhase::Compile)?;
    Ok(CompiledRegexShadowPlan { entries })
}

fn execute_compiled_plan(
    plan: &CompiledRegexShadowPlan,
    input: &str,
    control: ExecutionControl<'_>,
) -> Result<RegexShadowResult, RegexShadowFailure> {
    check_control(control, ExecutionPhase::Execute)?;
    if input.len() > 1_048_576 {
        return Err(RegexShadowFailure("regex_shadow_input_limit"));
    }
    let output_limit = 8_388_608usize.min(input.len().saturating_mul(4).saturating_add(65_536));
    let mut data = input.to_string();
    let mut errors = Vec::new();
    let mut total_matches = 0usize;
    for entry in &plan.entries {
        check_control(control, ExecutionPhase::Execute)?;
        let regex = match &entry.regex {
            Some(regex) => regex,
            None => {
                errors.push(RegexShadowRuleError {
                    source_index: entry.source_index,
                    category: "regex_shadow_compile",
                });
                continue;
            }
        };
        let original = data;
        let mut output = String::with_capacity(original.len());
        let mut end = 0;
        for captures in regex.captures_iter(&original) {
            total_matches += 1;
            if total_matches > 1_000_000 {
                return Err(RegexShadowFailure("regex_shadow_match_limit"));
            }
            if total_matches % 1_024 == 0 {
                check_control(control, ExecutionPhase::Execute)?;
            }
            let full_match = captures.get(0).expect("capture zero must exist");
            push_limited(
                &mut output,
                &original[end..full_match.start()],
                output_limit,
            )?;
            append_substitution(
                &mut output,
                &entry.replacement,
                &original,
                &captures,
                output_limit,
            )?;
            end = full_match.end();
            if !entry.global {
                break;
            }
        }
        push_limited(&mut output, &original[end..], output_limit)?;
        data = output;
    }
    Ok(RegexShadowResult { data, errors })
}

#[cfg(test)]
fn execute_json(
    plan_json: &str,
    input: &str,
    _cancelled: Option<&AtomicBool>,
) -> Result<RegexShadowResult, RegexShadowFailure> {
    let plan = serde_json::from_str(plan_json)
        .map_err(|_| RegexShadowFailure("regex_shadow_plan_json"))?;
    execute_plan_with_control(
        plan,
        input,
        ExecutionControl {
            cancelled: _cancelled,
            deadline: Some(Instant::now() + Duration::from_secs(2)),
            after_rule_compiled: None,
        },
    )
}

#[cfg(test)]
fn alternative(atoms: Vec<RegexShadowAtom>) -> RegexShadowAlternative {
    RegexShadowAlternative { atoms }
}

#[cfg(test)]
fn generated_plan(index: usize) -> RegexShadowPlan {
    let entries = match index % 4 {
        0 => vec![RegexShadowEntry {
            source_index: 0,
            global: true,
            capture_count: 2,
            pattern_bytes: 7,
            replacement_bytes: 4,
            pattern: RegexShadowPattern {
                alternatives: vec![
                    alternative(vec![RegexShadowAtom::Capture {
                        index: 1,
                        alternatives: vec![alternative(vec![RegexShadowAtom::Literal {
                            value: b'a',
                        }])],
                    }]),
                    alternative(vec![RegexShadowAtom::Capture {
                        index: 2,
                        alternatives: vec![alternative(vec![RegexShadowAtom::Literal {
                            value: b'b',
                        }])],
                    }]),
                ],
            },
            replacement: vec![
                ReplacementToken::Capture { index: 2 },
                ReplacementToken::Capture { index: 1 },
            ],
        }],
        1 => vec![RegexShadowEntry {
            source_index: 0,
            global: true,
            capture_count: 0,
            pattern_bytes: 10,
            replacement_bytes: 6,
            pattern: RegexShadowPattern {
                alternatives: vec![alternative(vec![RegexShadowAtom::Repeat {
                    min: 1,
                    max: 2,
                    atom: Box::new(RegexShadowAtom::Class {
                        ranges: vec![RegexShadowRange {
                            start: b'A',
                            end: b'Z',
                        }],
                    }),
                }])],
            },
            replacement: vec![
                ReplacementToken::Literal {
                    value: "$[".to_string(),
                },
                ReplacementToken::Match,
                ReplacementToken::Literal {
                    value: "]".to_string(),
                },
            ],
        }],
        2 => vec![RegexShadowEntry {
            source_index: 0,
            global: false,
            capture_count: 0,
            pattern_bytes: 12,
            replacement_bytes: 12,
            pattern: RegexShadowPattern {
                alternatives: vec![alternative(vec![RegexShadowAtom::Repeat {
                    min: 1,
                    max: 3,
                    atom: Box::new(RegexShadowAtom::Group {
                        alternatives: vec![
                            alternative(vec![RegexShadowAtom::Literal { value: b'x' }]),
                            alternative(vec![RegexShadowAtom::Literal { value: b'y' }]),
                        ],
                    }),
                }])],
            },
            replacement: vec![
                ReplacementToken::Literal {
                    value: "[".to_string(),
                },
                ReplacementToken::Prefix,
                ReplacementToken::Literal {
                    value: "][".to_string(),
                },
                ReplacementToken::Match,
                ReplacementToken::Literal {
                    value: "][".to_string(),
                },
                ReplacementToken::Suffix,
                ReplacementToken::Literal {
                    value: "]".to_string(),
                },
            ],
        }],
        _ => vec![
            RegexShadowEntry {
                source_index: 0,
                global: true,
                capture_count: 0,
                pattern_bytes: 7,
                replacement_bytes: 4,
                pattern: RegexShadowPattern {
                    alternatives: vec![alternative(vec![
                        RegexShadowAtom::Repeat {
                            min: 0,
                            max: 1,
                            atom: Box::new(RegexShadowAtom::Class {
                                ranges: vec![RegexShadowRange {
                                    start: b'0',
                                    end: b'9',
                                }],
                            }),
                        },
                        RegexShadowAtom::Literal { value: b'z' },
                    ])],
                },
                replacement: vec![ReplacementToken::Match, ReplacementToken::Match],
            },
            RegexShadowEntry {
                source_index: 1,
                global: true,
                capture_count: 0,
                pattern_bytes: 1,
                replacement_bytes: 1,
                pattern: RegexShadowPattern {
                    alternatives: vec![alternative(vec![RegexShadowAtom::Literal { value: b'z' }])],
                },
                replacement: vec![ReplacementToken::Literal {
                    value: "Z".to_string(),
                }],
            },
        ],
    };
    RegexShadowPlan {
        version: 1,
        entries,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        alternative, compare_shadow, compile_plan, compile_plan_with_control, execute_batch,
        execute_compiled_plan, execute_json, execute_plan, execute_plan_with_control,
        generated_plan, CompiledRegexShadowEntry, CompiledRegexShadowPlan, ExecutionControl,
        RegexCancellationRegistry, RegexShadowAlternative, RegexShadowAtom, RegexShadowEntry,
        RegexShadowPattern, RegexShadowPlan, ReplacementToken,
    };
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::io::{self, BufRead};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use std::time::Instant;

    fn literal_plan(replacement: String) -> RegexShadowPlan {
        RegexShadowPlan {
            version: 1,
            entries: vec![RegexShadowEntry {
                source_index: 0,
                global: true,
                capture_count: 0,
                pattern_bytes: 1,
                replacement_bytes: replacement.len(),
                pattern: RegexShadowPattern {
                    alternatives: vec![RegexShadowAlternative {
                        atoms: vec![RegexShadowAtom::Literal { value: b'a' }],
                    }],
                },
                replacement: vec![ReplacementToken::Literal { value: replacement }],
            }],
        }
    }

    fn nested_plan(atoms: Vec<RegexShadowAtom>) -> RegexShadowPlan {
        let mut alternatives = vec![RegexShadowAlternative { atoms }];
        for _ in 0..29 {
            let atom = RegexShadowAtom::Group { alternatives };
            alternatives = vec![RegexShadowAlternative { atoms: vec![atom] }];
        }
        let atom = alternatives.pop().unwrap().atoms.pop().unwrap();
        let mut plan = literal_plan("x".to_string());
        plan.entries[0].pattern_bytes = 4_096;
        plan.entries[0].pattern.alternatives[0].atoms[0] = atom;
        plan
    }

    #[test]
    fn batch_command_core_returns_only_the_complete_ordered_result() {
        let result = execute_batch(literal_plan("b".to_string()), "aa".to_string(), None).unwrap();

        assert_eq!(result.data, "bb");
        assert!(result.errors.is_empty());
    }

    #[test]
    fn cancellation_registry_targets_only_the_matching_active_request() {
        let registry = RegexCancellationRegistry::default();
        let execution = registry.register("request-a").unwrap();

        assert!(!registry.cancel("request-b").unwrap());
        assert!(registry.cancel("request-a").unwrap());
        assert!(execution
            .cancelled
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn cancellation_registration_is_released_when_execution_finishes() {
        let registry = RegexCancellationRegistry::default();
        {
            let _execution = registry.register("request-a").unwrap();
            assert_eq!(registry.requests.lock().unwrap().len(), 1);
        }

        assert!(registry.requests.lock().unwrap().is_empty());
        assert!(!registry.cancel("request-a").unwrap());
    }

    #[test]
    fn cancellation_registry_consumes_cancel_before_registration() {
        let registry = RegexCancellationRegistry::default();

        assert!(!registry.cancel("request-a").unwrap());
        let execution = registry.register("request-a").unwrap();

        assert!(execution
            .cancelled
            .load(std::sync::atomic::Ordering::Relaxed));
        drop(execution);
        assert!(registry.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_registry_expires_and_bounds_unmatched_cancellations() {
        let registry = RegexCancellationRegistry::default();
        let now = std::time::Instant::now();
        assert_eq!(
            registry
                .cancel_at(&"x".repeat(super::MAX_REGEX_REQUEST_ID_BYTES + 1), now)
                .unwrap_err(),
            "regex_shadow_request_id",
        );
        for index in 0..super::MAX_PENDING_REGEX_CANCELLATIONS {
            assert!(!registry
                .cancel_at(&format!("request-{index}"), now)
                .unwrap());
        }
        assert_eq!(
            registry.cancel_at("over-limit", now).unwrap_err(),
            "regex_shadow_registry_limit",
        );

        let expired_at = now + super::REGEX_PENDING_CANCELLATION_TTL;
        let execution = registry.register_at("request-0", expired_at).unwrap();

        assert!(!execution
            .cancelled
            .load(std::sync::atomic::Ordering::Relaxed));
        assert!(!registry.cancel_at("after-expiry", expired_at).unwrap());
    }

    #[test]
    fn executes_ordered_rules_with_ecmascript_substitution() {
        let plan = r#"{
            "version": 1,
            "entries": [
                {
                    "sourceIndex": 7,
                    "global": true,
                    "captureCount": 1,
                    "patternBytes": 4,
                    "replacementBytes": 3,
                    "pattern": {
                        "alternatives": [{
                            "atoms": [{
                                "kind": "capture",
                                "index": 1,
                                "alternatives": [{
                                    "atoms": [{"kind": "literal", "value": 97}]
                                }]
                            }]
                        }]
                    },
                    "replacement": [{"kind": "capture", "index": 1}, {"kind": "literal", "value": "b"}]
                },
                {
                    "sourceIndex": 3,
                    "global": false,
                    "captureCount": 0,
                    "patternBytes": 2,
                    "replacementBytes": 1,
                    "pattern": {
                        "alternatives": [{
                            "atoms": [{"kind": "literal", "value": 98}]
                        }]
                    },
                    "replacement": [{"kind": "literal", "value": "X"}]
                }
            ]
        }"#;

        let result = execute_json(plan, "aa", None).unwrap();

        assert_eq!(result.data, "aXab");
        assert!(result.errors.is_empty());
    }

    #[test]
    fn enforces_the_dynamic_output_limit() {
        let plan = literal_plan("x".repeat(100));

        let error = execute_plan(plan, &"a".repeat(20_000)).unwrap_err();

        assert_eq!(error.0, "regex_shadow_output_limit");
    }

    #[test]
    fn enforces_the_total_match_limit() {
        let plan = literal_plan("x".to_string());

        let error = execute_plan(plan, &"a".repeat(1_000_001)).unwrap_err();

        assert_eq!(error.0, "regex_shadow_match_limit");
    }

    #[test]
    fn reports_cancellation_during_compilation() {
        let cancelled = AtomicBool::new(true);
        let control = ExecutionControl {
            cancelled: Some(&cancelled),
            deadline: None,
            after_rule_compiled: None,
        };

        let error =
            execute_plan_with_control(literal_plan("x".to_string()), "a", control).unwrap_err();

        assert_eq!(error.0, "regex_shadow_cancelled_compile");
    }

    #[test]
    fn validates_classifier_resource_metadata() {
        let mut plan = literal_plan("x".to_string());
        plan.entries[0].pattern_bytes = 4_097;

        let error = execute_plan(plan, "a").unwrap_err();

        assert_eq!(error.0, "regex_shadow_pattern_limit");
    }

    #[test]
    fn rejects_malformed_neutral_ir() {
        let mut plan = literal_plan("x".to_string());
        plan.entries[0].capture_count = 1;
        plan.entries[0].pattern.alternatives[0].atoms[0] = RegexShadowAtom::Capture {
            index: 2,
            alternatives: vec![RegexShadowAlternative {
                atoms: vec![RegexShadowAtom::Literal { value: b'a' }],
            }],
        };

        let error = execute_plan(plan, "a").unwrap_err();

        assert_eq!(error.0, "regex_shadow_ir");
    }

    #[test]
    fn rejects_non_ascii_values_in_neutral_ir() {
        let mut literal = literal_plan("x".to_string());
        literal.entries[0].pattern.alternatives[0].atoms[0] =
            RegexShadowAtom::Literal { value: 0x80 };
        let literal_error = execute_plan(literal, "a").unwrap_err();

        let mut character_class = literal_plan("x".to_string());
        character_class.entries[0].pattern.alternatives[0].atoms[0] = RegexShadowAtom::Class {
            ranges: vec![super::RegexShadowRange {
                start: b'a',
                end: 0x80,
            }],
        };
        let class_error = execute_plan(character_class, "a").unwrap_err();

        assert_eq!(literal_error.0, "regex_shadow_ir");
        assert_eq!(class_error.0, "regex_shadow_ir");
    }

    #[test]
    fn rejects_ir_beyond_the_configured_nest_limit() {
        let mut atom = RegexShadowAtom::Literal { value: b'a' };
        for _ in 0..29 {
            atom = RegexShadowAtom::Group {
                alternatives: vec![RegexShadowAlternative { atoms: vec![atom] }],
            };
        }
        let mut plan = literal_plan("x".to_string());
        plan.entries[0].pattern_bytes = 4_096;
        plan.entries[0].pattern.alternatives[0].atoms[0] = atom;

        let boundary_result = execute_plan(plan, "a").unwrap();
        assert!(boundary_result.errors.is_empty());

        let mut atom = RegexShadowAtom::Literal { value: b'a' };
        for _ in 0..30 {
            atom = RegexShadowAtom::Group {
                alternatives: vec![RegexShadowAlternative { atoms: vec![atom] }],
            };
        }
        let mut plan = literal_plan("x".to_string());
        plan.entries[0].pattern_bytes = 4_096;
        plan.entries[0].pattern.alternatives[0].atoms[0] = atom;

        let error = execute_plan(plan, "a").unwrap_err();

        assert_eq!(error.0, "regex_shadow_nest_limit");
    }

    #[test]
    fn compiles_classifier_boundary_concatenation_through_json() {
        let plan = nested_plan(vec![
            RegexShadowAtom::Literal { value: b'a' },
            RegexShadowAtom::Literal { value: b'b' },
        ]);
        let plan_json = serde_json::to_string(&plan).unwrap();

        let result = execute_json(&plan_json, "ab", None).unwrap();

        assert!(result.errors.is_empty());
        assert_eq!(result.data, "x");
    }

    #[test]
    fn compiles_classifier_boundary_character_class_through_json() {
        let plan = nested_plan(vec![RegexShadowAtom::Class {
            ranges: vec![super::RegexShadowRange {
                start: b'a',
                end: b'b',
            }],
        }]);
        let plan_json = serde_json::to_string(&plan).unwrap();

        let result = execute_json(&plan_json, "a", None).unwrap();

        assert!(result.errors.is_empty());
        assert_eq!(result.data, "x");
    }

    #[test]
    fn compiles_classifier_boundary_with_nested_concat_and_alternation() {
        let mut atom = RegexShadowAtom::Class {
            ranges: vec![
                super::RegexShadowRange {
                    start: b'a',
                    end: b'b',
                },
                super::RegexShadowRange {
                    start: b'x',
                    end: b'z',
                },
            ],
        };
        for _ in 0..29 {
            atom = RegexShadowAtom::Group {
                alternatives: vec![
                    RegexShadowAlternative {
                        atoms: vec![RegexShadowAtom::Literal { value: b'x' }, atom],
                    },
                    RegexShadowAlternative {
                        atoms: vec![RegexShadowAtom::Literal { value: b'y' }],
                    },
                ],
            };
        }
        let mut plan = literal_plan("r".to_string());
        plan.entries[0].pattern_bytes = 4_096;
        plan.entries[0].pattern.alternatives[0].atoms[0] = atom;
        let plan_json = serde_json::to_string(&plan).unwrap();

        let result = execute_json(&plan_json, &format!("{}a", "x".repeat(29)), None).unwrap();

        assert!(result.errors.is_empty());
        assert_eq!(result.data, "r");
    }

    #[test]
    fn rejects_capture_ir_under_repetition() {
        let mut plan = literal_plan("x".to_string());
        plan.entries[0].capture_count = 1;
        plan.entries[0].pattern_bytes = 8;
        plan.entries[0].replacement_bytes = 2;
        plan.entries[0].pattern.alternatives[0].atoms[0] = RegexShadowAtom::Repeat {
            min: 1,
            max: 2,
            atom: Box::new(RegexShadowAtom::Capture {
                index: 1,
                alternatives: vec![RegexShadowAlternative {
                    atoms: vec![RegexShadowAtom::Literal { value: b'a' }],
                }],
            }),
        };
        plan.entries[0].replacement = vec![ReplacementToken::Capture { index: 1 }];

        let error = execute_plan(plan, "aa").unwrap_err();

        assert_eq!(error.0, "regex_shadow_capture_under_repeat");
    }

    #[test]
    fn checks_cancellation_after_each_rule_compilation() {
        let cancelled = AtomicBool::new(false);
        let after_compile = |index: usize| {
            if index == 0 {
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        };
        let control = ExecutionControl {
            cancelled: Some(&cancelled),
            deadline: None,
            after_rule_compiled: Some(&after_compile),
        };

        let error = execute_plan_with_control(phase_one_plan(20), "rule-000", control).unwrap_err();

        assert_eq!(error.0, "regex_shadow_cancelled_compile");
    }

    #[test]
    fn shadow_comparison_reports_only_bounded_evidence() {
        let plan = serde_json::to_string(&literal_plan("x".to_string())).unwrap();

        let evidence = compare_shadow("fixture-1", &plan, "a🙂", "x🙂", &[]);
        let serialized = serde_json::to_string(&evidence).unwrap();

        assert_eq!(evidence.category, "match");
        assert_eq!(evidence.first_differing_utf16_index, None);
        assert!(!serialized.contains("a🙂"));
        assert!(!serialized.contains("x🙂"));
    }

    #[test]
    fn preserves_compile_error_source_order() {
        let plan = CompiledRegexShadowPlan {
            entries: vec![
                CompiledRegexShadowEntry {
                    source_index: 9,
                    global: true,
                    replacement: Vec::new(),
                    regex: None,
                },
                CompiledRegexShadowEntry {
                    source_index: 3,
                    global: true,
                    replacement: Vec::new(),
                    regex: None,
                },
            ],
        };

        let result = execute_compiled_plan(
            &plan,
            "input",
            ExecutionControl {
                cancelled: None,
                deadline: None,
                after_rule_compiled: None,
            },
        )
        .unwrap();

        assert_eq!(
            result
                .errors
                .iter()
                .map(|error| error.source_index)
                .collect::<Vec<_>>(),
            vec![9, 3],
        );
    }

    #[test]
    fn reports_expired_deadlines_during_compilation() {
        let control = ExecutionControl {
            cancelled: None,
            deadline: Some(Instant::now()),
            after_rule_compiled: None,
        };

        let error =
            execute_plan_with_control(literal_plan("x".to_string()), "a", control).unwrap_err();

        assert_eq!(error.0, "regex_shadow_deadline_compile");
    }

    #[test]
    fn reports_cancellation_during_execution() {
        let plan = compile_plan(literal_plan("x".to_string())).unwrap();
        let cancelled = AtomicBool::new(true);

        let error = execute_compiled_plan(
            &plan,
            "a",
            ExecutionControl {
                cancelled: Some(&cancelled),
                deadline: None,
                after_rule_compiled: None,
            },
        )
        .unwrap_err();

        assert_eq!(error.0, "regex_shadow_cancelled_execute");
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DifferentialCase {
        id: u32,
        plan_json: String,
        input: String,
        authority_data: String,
        authority_error_source_indexes: Vec<usize>,
    }

    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct DifferentialSummary {
        allowed_cases: usize,
        unique_plans: usize,
        mismatches: usize,
        plan_hash: String,
        input_hash: String,
        authority_hash: String,
        rust_hash: String,
        first_mismatch: Option<super::RegexShadowEvidence>,
    }

    fn update_value_hash(hash: &mut Sha256, id: u32, value: &[u8]) {
        hash.update(id.to_le_bytes());
        hash.update((value.len() as u32).to_le_bytes());
        hash.update(value);
    }

    fn update_result_hash(hash: &mut Sha256, id: u32, data: &str, error_source_indexes: &[usize]) {
        update_value_hash(hash, id, data.as_bytes());
        hash.update((error_source_indexes.len() as u32).to_le_bytes());
        for source_index in error_source_indexes {
            hash.update((*source_index as u32).to_le_bytes());
        }
    }

    #[test]
    #[ignore = "profile-only JSONL differential harness"]
    fn jsonl_differential_harness() {
        let mut allowed_cases = 0usize;
        let mut mismatches = 0usize;
        let mut plan_hash = Sha256::new();
        let mut input_hash = Sha256::new();
        let mut authority_hash = Sha256::new();
        let mut rust_hash = Sha256::new();
        let mut first_mismatch = None;
        let mut compiled_plans = HashMap::new();

        for (line_index, line) in io::stdin().lock().lines().enumerate() {
            let line = line
                .unwrap_or_else(|_| panic!("failed to read differential line {}", line_index + 1));
            if line.is_empty() {
                continue;
            }
            let case: DifferentialCase = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("invalid differential case at line {}", line_index + 1));
            assert_eq!(case.id as usize, allowed_cases);
            allowed_cases += 1;
            update_value_hash(&mut plan_hash, case.id, case.plan_json.as_bytes());
            update_value_hash(&mut input_hash, case.id, case.input.as_bytes());
            update_result_hash(
                &mut authority_hash,
                case.id,
                &case.authority_data,
                &case.authority_error_source_indexes,
            );

            if !compiled_plans.contains_key(&case.plan_json) {
                let compiled = serde_json::from_str(&case.plan_json)
                    .map_err(|_| super::RegexShadowFailure("regex_shadow_plan_json"))
                    .and_then(|plan| {
                        compile_plan_with_control(
                            plan,
                            ExecutionControl {
                                cancelled: None,
                                deadline: Some(Instant::now() + Duration::from_secs(2)),
                                after_rule_compiled: None,
                            },
                        )
                    });
                match compiled {
                    Ok(plan) => {
                        compiled_plans.insert(case.plan_json.clone(), plan);
                    }
                    Err(_) => {
                        mismatches += 1;
                        if first_mismatch.is_none() {
                            first_mismatch = Some(compare_shadow(
                                &format!("generated-{}", case.id),
                                &case.plan_json,
                                &case.input,
                                &case.authority_data,
                                &case.authority_error_source_indexes,
                            ));
                        }
                        continue;
                    }
                }
            }
            let control = ExecutionControl {
                cancelled: None,
                deadline: Some(Instant::now() + Duration::from_secs(2)),
                after_rule_compiled: None,
            };
            match execute_compiled_plan(
                compiled_plans
                    .get(&case.plan_json)
                    .expect("compiled plan must be registered"),
                &case.input,
                control,
            ) {
                Ok(result) => {
                    let rust_error_source_indexes = result
                        .errors
                        .iter()
                        .map(|error| error.source_index)
                        .collect::<Vec<_>>();
                    update_result_hash(
                        &mut rust_hash,
                        case.id,
                        &result.data,
                        &rust_error_source_indexes,
                    );
                    if result.data != case.authority_data
                        || rust_error_source_indexes != case.authority_error_source_indexes
                    {
                        mismatches += 1;
                        if first_mismatch.is_none() {
                            first_mismatch = Some(compare_shadow(
                                &format!("generated-{}", case.id),
                                &case.plan_json,
                                &case.input,
                                &case.authority_data,
                                &case.authority_error_source_indexes,
                            ));
                        }
                    }
                }
                Err(_) => {
                    mismatches += 1;
                    if first_mismatch.is_none() {
                        first_mismatch = Some(compare_shadow(
                            &format!("generated-{}", case.id),
                            &case.plan_json,
                            &case.input,
                            &case.authority_data,
                            &case.authority_error_source_indexes,
                        ));
                    }
                }
            }
        }

        let summary = DifferentialSummary {
            allowed_cases,
            unique_plans: compiled_plans.len(),
            mismatches,
            plan_hash: format!("{:x}", plan_hash.finalize()),
            input_hash: format!("{:x}", input_hash.finalize()),
            authority_hash: format!("{:x}", authority_hash.finalize()),
            rust_hash: format!("{:x}", rust_hash.finalize()),
            first_mismatch,
        };
        println!(
            "RISUNEST_REGEX_DIFFERENTIAL {}",
            serde_json::to_string(&summary).expect("differential summary must serialize")
        );
        assert!(summary.allowed_cases > 0);
        assert_eq!(summary.mismatches, 0);
    }

    #[test]
    fn matches_the_javascript_authority_hash_for_generated_corpus() {
        let mut hash = Sha256::new();
        let plans = (0..4)
            .map(|index| compile_plan(generated_plan(index)).unwrap())
            .collect::<Vec<_>>();
        for index in 0..100_000u32 {
            let input = format!("🙂abABzxy.{}\r\n", index % 997);
            let result = execute_compiled_plan(
                &plans[index as usize % plans.len()],
                &input,
                ExecutionControl {
                    cancelled: None,
                    deadline: None,
                    after_rule_compiled: None,
                },
            )
            .unwrap();
            hash.update(index.to_le_bytes());
            hash.update((result.data.len() as u32).to_le_bytes());
            hash.update(result.data.as_bytes());
            hash.update((result.errors.len() as u32).to_le_bytes());
            for error in result.errors {
                hash.update((error.source_index as u32).to_le_bytes());
            }
        }

        assert_eq!(
            format!("{:x}", hash.finalize()),
            "6fc3f4ec46d9a5806b576b005aac64beba109e174d0e3e143dcb3cd93e19e4f5",
        );
    }

    fn phase_one_plan(rule_count: usize) -> RegexShadowPlan {
        RegexShadowPlan {
            version: 1,
            entries: (0..rule_count)
                .map(|index| {
                    let pattern = format!("rule-{index:03}");
                    let replacement = format!("done-{index:03}");
                    RegexShadowEntry {
                        source_index: index,
                        global: true,
                        capture_count: 0,
                        pattern_bytes: pattern.len(),
                        replacement_bytes: replacement.len(),
                        pattern: RegexShadowPattern {
                            alternatives: vec![alternative(
                                pattern
                                    .bytes()
                                    .map(|value| RegexShadowAtom::Literal { value })
                                    .collect(),
                            )],
                        },
                        replacement: vec![ReplacementToken::Literal { value: replacement }],
                    }
                })
                .collect(),
        }
    }

    fn phase_one_input(rule_count: usize, input_bytes: usize) -> String {
        let mut input = String::with_capacity(input_bytes);
        let mut index = 0usize;
        while input.len() < input_bytes {
            input.push_str(&format!("rule-{:03}|", index % rule_count));
            index += 1;
        }
        input.truncate(input_bytes);
        input
    }

    fn phase_one_fixture_input(rule_count: usize, target_bytes: usize) -> String {
        let mut input = String::with_capacity(target_bytes);
        let mut index = 0usize;
        while input.len() < target_bytes {
            input.push_str(&format!("rule-{:03}|", index % rule_count));
            index += 1;
        }
        input
    }

    fn fnv1a_utf16(value: &str) -> String {
        let hash = value.encode_utf16().fold(0x811c9dc5u32, |hash, unit| {
            (hash ^ u32::from(unit)).wrapping_mul(0x01000193)
        });
        format!("{hash:08x}")
    }

    #[test]
    fn matches_current_phase_one_fixture_hashes() {
        for (rule_count, expected_hash) in [(20, "5e460344"), (100, "d3272578"), (500, "b61dac41")]
        {
            let plan = compile_plan(phase_one_plan(rule_count)).unwrap();
            let input = phase_one_fixture_input(rule_count, 32 * 1024);
            let result = execute_compiled_plan(
                &plan,
                &input,
                ExecutionControl {
                    cancelled: None,
                    deadline: None,
                    after_rule_compiled: None,
                },
            )
            .unwrap();

            assert!(result.errors.is_empty());
            assert_eq!(fnv1a_utf16(&result.data), expected_hash);
        }
    }

    #[test]
    #[ignore = "Windows profile benchmark"]
    fn windows_profile_benchmark() {
        for rule_count in [20, 100, 500] {
            let plan = compile_plan(phase_one_plan(rule_count)).unwrap();
            for input_bytes in [32 * 1024, 256 * 1024, 1024 * 1024] {
                let input = phase_one_input(rule_count, input_bytes);
                let mut samples = Vec::with_capacity(10);
                for run in 0..11 {
                    let started = Instant::now();
                    let result = execute_compiled_plan(
                        &plan,
                        &input,
                        ExecutionControl {
                            cancelled: None,
                            deadline: None,
                            after_rule_compiled: None,
                        },
                    )
                    .unwrap();
                    assert!(result.errors.is_empty());
                    assert_eq!(result.data.len(), input.len());
                    if run != 0 {
                        samples.push(started.elapsed());
                    }
                }
                samples.sort_unstable();
                let p50 = samples[4];
                let p95 = samples[9];
                println!(
                    "{{\"engine\":\"rust_regex\",\"rules\":{rule_count},\"inputBytes\":{input_bytes},\"samples\":10,\"p50Micros\":{},\"p95Micros\":{}}}",
                    duration_micros(p50),
                    duration_micros(p95),
                );
            }
        }
    }

    fn duration_micros(duration: Duration) -> u128 {
        duration.as_micros()
    }
}
