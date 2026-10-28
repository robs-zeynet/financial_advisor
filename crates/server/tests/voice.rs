//! Voice pipeline end to end over a real WebSocket, with fake speech engines
//! and a mock Messages API: VAD, speech-to-text, spoken replies, barge-in,
//! push-to-talk, and spoken commands.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use protocol::{ClientMsg, ListenState, ServerMsg, SpokenCommand, TtsHeader};
use serde_json::{json, Value};
use server::session::Voice;

const QUIET: f32 = -90.0;
const SPEECH: f32 = -20.0;

fn voice(stt: &Arc<FakeStt>, tts: &Arc<FakeTts>) -> Voice {
    Voice { stt: Some(stt.clone()), tts: Some(tts.clone()) }
}

/// Speaks one utterance: a little silence, `ms` of "speech", then enough
/// silence for the VAD to end it.
async fn say(c: &mut FakeClient, ms: u32) {
    c.send_mic(&mic_audio(300, QUIET)).await;
    c.send_mic(&mic_audio(ms, SPEECH)).await;
    c.send_mic(&mic_audio(900, QUIET)).await;
}

fn messages(body: &Value) -> &Vec<Value> {
    body["messages"].as_array().unwrap()
}

#[tokio::test]
async fn spoken_question_gets_a_spoken_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Sse("text.sse")]).await;
    let stt = FakeStt::new(&["What industries pay regular dividends?"]);
    let tts = FakeTts::new(0.5);
    let (addr, _) = spawn_app_with_voice(test_config(dir.path(), &mock.base_url), voice(&stt, &tts)).await;
    let mut c = FakeClient::connect(addr).await;
    c.send(ClientMsg::StartSession).await;
    match c.recv().await {
        ServerMsg::SessionStarted { voice, .. } => assert!(voice.stt && voice.tts),
        other => panic!("{other:?}"),
    }

    say(&mut c, 800).await;
    let seen = c.recv_until(done).await;
    let listen: Vec<ListenState> = seen
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Listen { state } => Some(*state),
            _ => None,
        })
        .collect();
    assert_eq!(listen, vec![ListenState::Hearing, ListenState::Transcribing, ListenState::Idle]);
    assert!(seen.contains(&ServerMsg::UserTurn { text: "What industries pay regular dividends?".into() }));

    // The utterance handed to STT covers the speech plus pre-roll and trailing silence.
    let heard = stt.heard_samples.lock().unwrap()[0];
    assert!((800 * 16..=2000 * 16).contains(&heard), "utterance of {heard} samples");

    // The reply was spoken: one sentence, sent as TTS audio.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(tts.spoken.lock().unwrap().as_slice(), ["Utilities and consumer staples."]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while c.audio.is_empty() && tokio::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(100), c.recv()).await;
    }
    let (h, pcm) = TtsHeader::decode(&c.audio[0]).unwrap();
    assert_eq!((h.utterance_id, h.sentence_index, h.sample_rate), (1, 0, 16_000));
    assert_eq!(pcm.len(), 8_000 * 2);

    let reqs = mock.requests();
    assert_eq!(messages(&reqs[0].body)[0]["content"][0]["text"], "What industries pay regular dividends?");
}

/// Barge-in while the reply is still streaming: the turn is cancelled and
/// only the sentence the user heard is recorded.
#[tokio::test]
async fn barge_in_mid_stream_records_only_what_was_heard() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::SlowSse("long_text.sse", 150), MockReply::Sse("text.sse")]).await;
    let stt = FakeStt::new(&["Tell me about bonds.", "Wait, what about stocks?"]);
    let tts = FakeTts::new(3.0);
    let (addr, _) = spawn_app_with_voice(test_config(dir.path(), &mock.base_url), voice(&stt, &tts)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;

    say(&mut c, 600).await;
    // Wait until the first sentence's audio arrives.
    while c.audio.is_empty() {
        c.recv().await;
    }
    // Talk over it.
    say(&mut c, 700).await;
    let seen = c.recv_until(|m| matches!(m, ServerMsg::UserTurn { text } if text.starts_with("Wait"))).await;
    assert!(seen.iter().any(|m| matches!(m, ServerMsg::StopAudio { utterance_id: 1 })), "playback stopped");
    assert!(seen.contains(&ServerMsg::AssistantDone { stop_reason: "cancelled".into() }));
    c.recv_until(done).await;

    let reqs = mock.requests();
    let history = messages(&reqs[1].body);
    let assistant: Vec<&Value> = history.iter().filter(|m| m["role"] == "assistant").collect();
    assert_eq!(assistant.len(), 1);
    assert_eq!(assistant[0]["content"], json!([{"type": "text", "text": "Bonds pay fixed interest."}]));
}

/// Barge-in after the reply finished streaming but while it was still being
/// spoken: history stays append-only, and a system note says what was heard.
#[tokio::test]
async fn barge_in_after_streaming_adds_a_heard_note() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Sse("long_text.sse"), MockReply::Sse("text.sse")]).await;
    let stt = FakeStt::new(&["Tell me about bonds.", "Okay, and stocks?"]);
    let tts = FakeTts::new(3.0);
    let (addr, _) = spawn_app_with_voice(test_config(dir.path(), &mock.base_url), voice(&stt, &tts)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;

    say(&mut c, 600).await;
    c.recv_until(done).await;
    while c.audio.is_empty() {
        c.recv().await;
    }
    say(&mut c, 700).await;
    let seen = c.recv_until(done).await;
    assert!(seen.iter().any(|m| matches!(m, ServerMsg::StopAudio { .. })));

    let reqs = mock.requests();
    let history = messages(&reqs[1].body);
    // The full reply stays in history unedited...
    let reply = history.iter().find(|m| m["role"] == "assistant").unwrap();
    assert!(reply["content"][0]["text"].as_str().unwrap().ends_with("sensitive to rates."));
    // ...and the note after the next user message says only the first sentence was heard.
    let note = history.iter().rev().find(|m| m["role"] == "system").unwrap();
    let text = note["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"Bonds pay fixed interest.\""), "{text}");
    assert!(!text.contains("less volatile"), "{text}");
}

#[tokio::test]
async fn spoken_scan_commands_are_handled_locally() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Sse("text.sse")]).await;
    let stt = FakeStt::new(&["Okay, scan this page.", "Done.", "Done."]);
    let tts = FakeTts::new(0.2);
    let (addr, _) = spawn_app_with_voice(test_config(dir.path(), &mock.base_url), voice(&stt, &tts)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;

    say(&mut c, 500).await;
    let seen = c.recv_until(|m| matches!(m, ServerMsg::CommandHeard { .. })).await;
    assert!(seen.contains(&ServerMsg::CommandHeard { text: "Okay, scan this page.".into(), command: SpokenCommand::StartScan }));

    // The browser starts the scan; "done" now ends it, locally.
    c.start_scan(1).await;
    say(&mut c, 400).await;
    let seen = c.recv_until(|m| matches!(m, ServerMsg::CommandHeard { .. })).await;
    assert!(seen.iter().any(|m| matches!(m, ServerMsg::CommandHeard { command: SpokenCommand::EndScan, .. })));
    c.send(ClientMsg::EndScan { scan_id: 1 }).await;

    // With no scan running, "done" is just something to tell the advisor.
    say(&mut c, 400).await;
    let seen = c.recv_until(done).await;
    assert!(seen.contains(&ServerMsg::UserTurn { text: "Done.".into() }));
    assert_eq!(mock.requests().len(), 1, "only the last utterance reached Claude");
}

#[tokio::test]
async fn push_to_talk_and_mic_off() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Sse("text.sse")]).await;
    let stt = FakeStt::new(&["Push to talk works."]);
    let tts = FakeTts::new(0.2);
    let (addr, _) = spawn_app_with_voice(test_config(dir.path(), &mock.base_url), voice(&stt, &tts)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;

    // Mic off: audio is ignored entirely.
    c.send(ClientMsg::MicState { on: false }).await;
    assert_eq!(c.recv().await, ServerMsg::Listen { state: ListenState::Idle });
    say(&mut c, 800).await;
    c.send(ClientMsg::MicState { on: true }).await;

    // Push-to-talk: quiet speech and long pauses still make one utterance.
    c.send(ClientMsg::PttDown).await;
    assert_eq!(c.recv().await, ServerMsg::Listen { state: ListenState::Hearing });
    c.send_mic(&mic_audio(1500, QUIET)).await;
    c.send(ClientMsg::PttUp).await;
    let seen = c.recv_until(done).await;
    assert!(seen.contains(&ServerMsg::UserTurn { text: "Push to talk works.".into() }));
    let heard = stt.heard_samples.lock().unwrap().clone();
    assert_eq!(heard.len(), 1, "the ignored audio never reached STT");
    assert!(heard[0] >= 1500 * 16, "the whole held span was transcribed");
}
