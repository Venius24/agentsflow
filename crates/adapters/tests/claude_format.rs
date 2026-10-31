//! A turn recorded from a real Claude Code run (2.1.316, `-p --output-format stream-json`).
//! If a Claude update changes the output format, this test fails before the owner notices in a live run.

use agentsflow_adapters::claude::map_output;
use agentsflow_events::{Event, Role};
use claude_codes::ClaudeOutput;

#[test]
fn recorded_real_turn_maps_to_events() {
    let events: Vec<Event> = include_str!("fixtures/claude_turn.jsonl")
        .lines()
        .flat_map(|line| {
            let output = ClaudeOutput::parse_json_tolerant(line).expect("line parses");
            map_output(&output)
        })
        .collect();
    assert_eq!(
        events,
        vec![
            Event::Message {
                role: Role::Assistant,
                text: "ok".into(),
                delta: false
            },
            Event::Usage {
                input_tokens: 2 + 11874 + 16495,
                output_tokens: 4,
                cost_usd: Some(0.00254195)
            },
            Event::Done {
                reason: "success".into()
            },
        ]
    );
}
