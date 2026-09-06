//! P1.2: result spill (§7.4) — oversized JSON and Blocks, head/tail at UTF-8 boundaries,
//! artifact handles, store failure, cap clamping.

mod support;

use std::sync::Arc;

use kernel::loop_::spill::{head_utf8, tail_utf8};
use kernel::*;
use serde_json::json;
use support::*;

fn big_text() -> String {
    // 3-byte chars so byte cuts land inside characters: 1500 × 3 = 4500 bytes > 1 KiB cap.
    "€".repeat(1500)
}

fn small_spill() -> SpillConfig {
    SpillConfig {
        cap_bytes: 1024,
        head_bytes: 100,
        tail_bytes: 50,
    }
}

#[test]
fn head_and_tail_cut_at_utf8_boundaries() {
    let s = "a€b€c"; // bytes: a(1) €(3) b(1) €(3) c(1) = 9
    assert_eq!(head_utf8(s, 2), "a");
    assert_eq!(head_utf8(s, 4), "a€");
    assert_eq!(head_utf8(s, 100), s);
    assert_eq!(tail_utf8(s, 2), "c");
    assert_eq!(tail_utf8(s, 4), "€c");
    assert_eq!(tail_utf8(s, 0), "");
}

#[tokio::test]
async fn oversized_json_result_is_spilled() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "big", json!({}))]),
        text_response("done"),
    ]);
    let big = ValueTool::new("big", json!({"stdout": big_text()}));
    let mut setup = Setup::new(provider.clone()).tool(big);
    setup.spill = small_spill();
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let res = tool_results(&events)[0];
    assert!(res.spilled);
    assert!(!res.is_error);
    let full = canonical_json(&json!({"stdout": big_text()})).unwrap();
    let handle = ArtifactHandle(Hash::of_bytes(&full));
    assert_eq!(res.artifact_handles, vec![handle.clone()]);
    let spill = res.spill.as_ref().unwrap();
    assert_eq!(spill.handle, handle);
    assert_eq!(spill.size, full.len() as u64);
    assert_eq!(spill.mime, "application/json");
    let ToolResultContent::Json(v) = &res.content else {
        panic!()
    };
    let spilled: Spilled = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(spilled.handle, handle);
    assert_eq!(spilled.size, full.len() as u64);
    assert!(spilled.head.len() <= 100 && spilled.head.len() >= 98);
    assert!(spilled.tail.len() <= 50 && spilled.tail.len() >= 48);
    assert!(spilled.head.starts_with("{\"stdout\":\"€"));
    assert!(spilled.tail.ends_with("€\"}"));
    // result_hash is the hash of the post-spill content.
    assert_eq!(
        res.result_hash,
        Hash::of_canonical_json(&json!({"content": {"json": v}, "is_error": false})).unwrap()
    );
    // The model-visible message carries the Spilled shape, not the full bytes.
    let ContentBlock::ToolResult {
        content: ToolResultContent::Json(seen),
        ..
    } = &k.state().messages[2].content[0]
    else {
        panic!()
    };
    assert_eq!(seen, v);
    let ctx = serde_json::to_string(&provider.requests()[1].messages).unwrap();
    assert!(
        ctx.len() < 2000,
        "oversized content never entered the context"
    );
}

#[tokio::test]
async fn oversized_text_blocks_are_spilled_independently() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "blocks", json!({}))]),
        text_response("done"),
    ]);
    let tool = ValueTool::with_result(
        "blocks",
        Ok(ToolResult::Blocks(vec![
            ContentBlock::Text {
                text: "small".into(),
            },
            ContentBlock::Text { text: big_text() },
            ContentBlock::Image {
                artifact_handle: ArtifactHandle(h("ab")),
                mime: "image/png".into(),
            },
        ])),
    );
    let mut setup = Setup::new(provider).tool(tool);
    setup.spill = small_spill();
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    let events = setup.events();
    let res = tool_results(&events)[0];
    assert!(res.spilled);
    let handle = ArtifactHandle(Hash::of_bytes(big_text().as_bytes()));
    assert_eq!(
        res.artifact_handles,
        vec![ArtifactHandle(h("ab")), handle.clone()]
    );
    assert_eq!(
        res.spill.as_ref().unwrap().mime,
        "text/plain; charset=utf-8"
    );
    let ToolResultContent::Blocks(blocks) = &res.content else {
        panic!()
    };
    assert_eq!(
        blocks[0],
        ContentBlock::Text {
            text: "small".into()
        }
    );
    let ContentBlock::Text { text } = &blocks[1] else {
        panic!()
    };
    let spilled: Spilled = serde_json::from_str(text).unwrap();
    assert_eq!(spilled.handle, handle);
    assert_eq!(spilled.head.chars().count(), 33);
    assert!(matches!(blocks[2], ContentBlock::Image { .. }));
}

#[tokio::test]
async fn store_failure_is_an_error_result_with_head_and_tail_only() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "big", json!({}))]),
        text_response("done"),
    ]);
    let big = ValueTool::new("big", json!(big_text()));
    let mut setup = Setup::new(provider.clone()).tool(big);
    setup.spill = small_spill();
    setup.artifact_store = Arc::new(FailingArtifactStore);
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    let events = setup.events();
    let res = tool_results(&events)[0];
    assert!(res.spilled);
    assert!(res.is_error);
    assert!(res.spill.is_none());
    assert!(res.artifact_handles.is_empty());
    let ToolResultContent::Json(v) = &res.content else {
        panic!()
    };
    assert!(v.get("handle").is_none());
    assert!(v["head"].as_str().unwrap().len() <= 100);
    assert!(v["tail"].as_str().unwrap().len() <= 50);
    assert!(warning_classes(&events).contains(&"spill_store_failed".to_owned()));
    let ctx = serde_json::to_string(&provider.requests()[1].messages).unwrap();
    assert!(
        !ctx.contains(&"€".repeat(200)),
        "oversized content never entered the context"
    );
}

#[tokio::test]
async fn spill_cap_is_clamped_with_a_warning_and_tasks_never_spill() {
    let provider = FakeProvider::responses(vec![text_response("x")]);
    let mut setup = Setup::new(provider);
    setup.spill = SpillConfig {
        cap_bytes: 0,
        head_bytes: 10,
        tail_bytes: 10,
    };
    let _k = setup.create().await;
    let events = setup.events();
    assert!(warning_classes(&events).contains(&"spill_cap_clamped".to_owned()));
    let created: Vec<&SessionCreatedPayload> = find_all(&events, |b| match b {
        EventBody::SessionCreated(p) => Some(p),
        _ => None,
    });
    assert_eq!(created[0].spill.cap_bytes, SpillConfig::MIN_CAP_BYTES);
    // Upper clamp.
    let mut setup = Setup::new(FakeProvider::responses(vec![]));
    setup.spill = SpillConfig {
        cap_bytes: u64::MAX,
        head_bytes: 10,
        tail_bytes: 10,
    };
    let _k = setup.create().await;
    let events = setup.events();
    let created: Vec<&SessionCreatedPayload> = find_all(&events, |b| match b {
        EventBody::SessionCreated(p) => Some(p),
        _ => None,
    });
    assert_eq!(created[0].spill.cap_bytes, SpillConfig::MAX_CAP_BYTES);
}
