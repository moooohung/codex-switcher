import assert from "node:assert/strict";
import test, { type TestContext } from "node:test";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import ts from "typescript";
import { renderToStaticMarkup } from "react-dom/server";
import * as promptCache from "../src/lib/promptCache.ts";
import {
  formatPromptCacheDuration,
  formatPromptCacheScope,
  getPromptCacheAutoKeepalive,
  getPromptCacheObservation,
  getPromptCacheReason,
  getPromptCacheSessionError,
  getPromptCacheSessionReason,
  getPromptCacheTtl,
  PROMPT_CACHE_WARMUP_NOTICE,
  PROMPT_CACHE_AUTO_KEEPALIVE_NOTICE,
} from "../src/lib/promptCache.ts";
import type { PromptCacheProvider, PromptCacheSessionList, PromptCacheStatus } from "../src/lib/promptCache.ts";

const REQUEST_AT = 1_800_000_000;

test("keepalive labels distinguish every backend state without claiming a cache hit", () => {
  const labels = {
    off: "꺼짐", waiting_session: "세션 연결 대기", idle: "요청 대기", busy: "대화 사용 중",
    waiting_for_stop: "응답 완료 대기", timer_waiting: "유지 타이머 연결 대기",
    scheduled: "요청 예약", pending: "요청 준비 대기", expired: "캐시 만료",
    error: "유지 요청 오류", unsupported: "현재 경로 미지원",
  };
  for (const [state, label] of Object.entries(labels)) {
    const presentation = getPromptCacheAutoKeepalive(observed({
      auto_keepalive_enabled: true, auto_keepalive_supported: true, auto_keepalive_state: state,
    }), REQUEST_AT);
    assert.equal(presentation.state, state);
    assert.equal(presentation.label, label);
    assert.equal(presentation.requests, null);
    assert.equal(presentation.outputTokens, null);
    assert.equal(presentation.cacheReadTokens, null);
    assert.equal(presentation.hasUsage, false);
  }
});

test("Claude response failure remains a registered live session and never claims a failed maintenance request", () => {
  const presentation = getPromptCacheAutoKeepalive(observed({
    auto_keepalive_enabled: true, auto_keepalive_supported: true,
    auto_keepalive_registered_sessions: 1, auto_keepalive_busy_sessions: 1,
    auto_keepalive_connected_sessions: 0, auto_keepalive_requests: 0,
    auto_keepalive_state: "failed",
    auto_keepalive_reason: "Claude 응답 오류 뒤 자동 유지를 보류했습니다. 새 사용자 입력과 정상 응답 완료가 확인되면 같은 세션에서 복구합니다.",
  }), REQUEST_AT, "claude");
  assert.equal(presentation.label, "Claude 응답 오류 · 복구 대기");
  assert.match(presentation.connectionNotice, /감지된 열린 Claude 세션 1개/);
  assert.doesNotMatch(presentation.connectionNotice, /최초 연결|유지 요청 오류/);
  assert.equal(presentation.requests, 0);
  assert.equal(presentation.nextAt, null);
  assert.equal(presentation.connectedSessions, 0);
});

test("disabled, missing and unknown preferences do not imply active automatic maintenance", () => {
  const stale = observed({ auto_keepalive_enabled: false, auto_keepalive_state: "scheduled", auto_keepalive_next_at: REQUEST_AT + 60 });
  assert.equal(getPromptCacheAutoKeepalive(stale).label, "꺼짐");
  assert.equal(getPromptCacheAutoKeepalive(stale).nextAt, null);
  for (const status of [null, undefined, observed()]) {
    const presentation = getPromptCacheAutoKeepalive(status);
    assert.equal(presentation.enabled, false);
    assert.equal(presentation.supported, false);
    assert.equal(presentation.label, "설정 확인 불가");
  }
  for (const auto_keepalive_supported of [true, false, undefined]) {
    for (const auto_keepalive_state of [undefined, "future_state", "__proto__", "constructor"]) {
      const presentation = getPromptCacheAutoKeepalive(observed({
        auto_keepalive_enabled: true, auto_keepalive_supported, auto_keepalive_state,
      }));
      assert.equal(presentation.state, "unknown");
      assert.equal(presentation.label, "상태 확인 불가");
      assert.doesNotMatch(presentation.label, /미지원|성공/);
    }
  }
});

test("verified living connections explain automatic application independently of preference and cache evidence", () => {
  for (const provider of ["codex", "claude"] as const) {
    for (const count of [1, 2, Number.MAX_SAFE_INTEGER]) {
      const status = observed({ state: "unknown", auto_keepalive_connected_sessions: count,
        auto_keepalive_registered_sessions: 2, auto_keepalive_busy_sessions: 1,
        auto_keepalive_enabled: true, auto_keepalive_state: "waiting_for_request",
        observed_at: null, cache_read_tokens: null, cache_write_tokens: null,
        uncached_input_tokens: null, expires_at: null, ttl_seconds: null });
      const on = getPromptCacheAutoKeepalive(status, REQUEST_AT, provider);
      assert.equal(on.connectedSessions, count);
      assert.equal(on.connectionNotice, `연결된 열린 세션 ${count}개에 자동 적용 · 다시 선택할 필요 없음`);
      assert.equal(on.label, "첫 요청 대기");
      assert.equal(on.hasUsage, false);
      assert.equal(on.outputTokens, null);
      assert.equal(on.cacheReadTokens, null);
      assert.equal(on.nextRequestLabel, null);
      assert.equal(getPromptCacheObservation(status, REQUEST_AT), null);
      assert.equal(getPromptCacheTtl(status, REQUEST_AT), null);
      const off = getPromptCacheAutoKeepalive({ ...status, auto_keepalive_enabled: false }, REQUEST_AT, provider);
      assert.equal(off.connectionNotice, `연결된 열린 세션 ${count}개 · 옵션을 켜면 자동 적용`);
      assert.equal(off.label, "꺼짐");
    }
  }
});

test("missing or malformed connection counts never infer connections from cache hits, requests or adapter state", () => {
  const waiting = "열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다.";
  for (const count of [undefined, null, "2", true, false, -1, 0.5, NaN, Infinity, -Infinity,
    Number.MAX_SAFE_INTEGER + 1, {}, [2]]) {
    for (const enabled of [true, false]) {
      const status = observed({ auto_keepalive_enabled: enabled, auto_keepalive_supported: true,
        auto_keepalive_state: "active", auto_keepalive_requests: 10, auto_keepalive_output_tokens: 0,
        auto_keepalive_cache_read_tokens: 8192, auto_keepalive_connected_sessions: count as number });
      const presentation = getPromptCacheAutoKeepalive(status, REQUEST_AT);
      assert.equal(presentation.connectedSessions, null);
      assert.equal(presentation.connectionNotice, waiting);
    }
  }
  for (const status of [null, undefined, observed(), observed({ auto_keepalive_connected_sessions: 0 })]) {
    assert.equal(getPromptCacheAutoKeepalive(status).connectionNotice, waiting);
  }
});

test("connection evidence neither invents an unknown preference nor overrides unsupported route status", () => {
  for (const enabled of [undefined, null, "true", 1, {}]) {
    const presentation = getPromptCacheAutoKeepalive(observed({ auto_keepalive_connected_sessions: 2,
      auto_keepalive_enabled: enabled as boolean, auto_keepalive_state: "active" }));
    assert.equal(presentation.enabled, false);
    assert.equal(presentation.label, "설정 확인 불가");
    assert.equal(presentation.connectionNotice, "연결된 열린 세션 2개 · 자동 유지 설정 확인 불가");
  }
  for (const count of [0, 2]) {
    const presentation = getPromptCacheAutoKeepalive(observed({ auto_keepalive_enabled: true,
      auto_keepalive_connected_sessions: count, auto_keepalive_supported: false,
      auto_keepalive_state: "unsupported", auto_keepalive_reason: "codex_web_unsupported" }), REQUEST_AT, "codex");
    assert.equal(presentation.connectedSessions, count);
    assert.equal(presentation.label, "현재 경로 미지원");
    assert.match(presentation.tooltip, /Codex 웹 경로는 지원하지 않/);
    assert.equal(presentation.hasUsage, false);
    assert.equal(presentation.outputTokens, null);
  }
});

test("registered busy Claude sessions wait for response completion without inventing a timer or cache evidence", () => {
  const status = Object.freeze(observed({ state: "unknown", auto_keepalive_enabled: true,
    auto_keepalive_supported: true, auto_keepalive_reason: "claude_official_live_hooks",
    auto_keepalive_state: "busy", auto_keepalive_connected_sessions: 0,
    auto_keepalive_registered_sessions: 3, auto_keepalive_busy_sessions: 2,
    auto_keepalive_next_at: REQUEST_AT + 60, observed_at: null, expires_at: null, ttl_seconds: null }));
  const presentation = getPromptCacheAutoKeepalive(status, REQUEST_AT, "claude");
  assert.equal(presentation.connectedSessions, 0);
  assert.equal(presentation.registeredSessions, 3);
  assert.equal(presentation.busySessions, 2);
  assert.equal(presentation.connectionNotice, "감지된 열린 Claude 세션 3개 · 작업 중 2개 · 응답 완료 후 자동 유지 대기");
  assert.equal(presentation.state, "busy");
  assert.equal(presentation.label, "대화 사용 중");
  assert.equal(presentation.nextAt, null);
  assert.equal(presentation.nextRequestLabel, null);
  assert.equal(presentation.hasUsage, false);
  assert.equal(getPromptCacheObservation(status, REQUEST_AT), null);
  assert.equal(getPromptCacheTtl(status, REQUEST_AT), null);
  assert.doesNotMatch(presentation.connectionNotice, /다음.*입력|최초 연결|자동 적용/);
});

test("registered Claude response and idle timer waits remain distinct without a schedule", () => {
  for (const [state, label, notice] of [
    ["waiting_for_stop", "응답 완료 대기", "감지된 열린 Claude 세션 2개 · 응답 완료 후 자동 유지 대기"],
    ["timer_waiting", "유지 타이머 연결 대기", "감지된 열린 Claude 세션 2개 · 유지 타이머 연결 대기"],
  ]) {
    const presentation = getPromptCacheAutoKeepalive(observed({ auto_keepalive_enabled: true,
      auto_keepalive_supported: true, auto_keepalive_reason: state, auto_keepalive_state: state,
      auto_keepalive_connected_sessions: 0, auto_keepalive_registered_sessions: 2,
      auto_keepalive_busy_sessions: 0, auto_keepalive_next_at: REQUEST_AT + 60 }), REQUEST_AT, "claude");
    assert.equal(presentation.connectionNotice, notice);
    assert.equal(presentation.state, state);
    assert.equal(presentation.label, label);
    assert.equal(presentation.connectedSessions, 0);
    assert.equal(presentation.nextAt, null);
    assert.equal(presentation.nextRequestLabel, null);
    assert.equal(presentation.hasUsage, false);
    assert.doesNotMatch(presentation.connectionNotice, /다음.*입력|최초 연결|자동 적용/);
    assert.doesNotMatch(presentation.tooltip, /서버의 유지 경로 사유를 확인할 수 없습니다/);
  }
});

test("missing and invalid registration counts preserve legacy handling without inferring registration", () => {
  const status = observed({ auto_keepalive_enabled: true, auto_keepalive_supported: true,
    auto_keepalive_reason: "claude_official_live_hooks", auto_keepalive_state: "busy",
    auto_keepalive_connected_sessions: 0, auto_keepalive_busy_sessions: 1 });
  const legacy = "열린 Claude 세션 등록 대기 · 일반 사용자 입력으로 세션을 등록합니다.";
  for (const count of [undefined, null, "2", true, false, -1, 0.5, NaN, Infinity, -Infinity,
    0x1_0000_0000, Number.MAX_SAFE_INTEGER, {}, [2]]) {
    const presentation = getPromptCacheAutoKeepalive({ ...status,
      auto_keepalive_registered_sessions: count as number }, REQUEST_AT, "claude");
    assert.equal(presentation.registeredSessions, null);
    assert.equal(presentation.connectedSessions, 0);
    assert.equal(presentation.connectionNotice, legacy);
  }
  const zero = getPromptCacheAutoKeepalive({ ...status, auto_keepalive_registered_sessions: 0 }, REQUEST_AT, "claude");
  assert.equal(zero.registeredSessions, 0);
  assert.equal(zero.busySessions, null);
  assert.equal(zero.connectionNotice, legacy);
  const missing = getPromptCacheAutoKeepalive({ ...status, auto_keepalive_busy_sessions: undefined }, REQUEST_AT, "claude");
  assert.equal(missing.registeredSessions, null);
  assert.equal(missing.busySessions, null);
  assert.equal(missing.connectionNotice, legacy);
  const max = getPromptCacheAutoKeepalive({ ...status, auto_keepalive_registered_sessions: 0xffff_ffff,
    auto_keepalive_busy_sessions: 0xffff_ffff }, REQUEST_AT, "claude");
  assert.equal(max.registeredSessions, 0xffff_ffff);
  assert.equal(max.busySessions, 0xffff_ffff);
  assert.equal(max.connectedSessions, 0);
});

test("registered sessions with unknown or inconsistent busy evidence never demand another input", () => {
  const status = observed({ auto_keepalive_enabled: true, auto_keepalive_supported: true,
    auto_keepalive_reason: "claude_official_live_hooks", auto_keepalive_state: "timer_waiting",
    auto_keepalive_connected_sessions: 0, auto_keepalive_registered_sessions: 2 });
  for (const count of [undefined, null, "1", true, false, -1, 0.5, NaN, Infinity, -Infinity,
    3, 0x1_0000_0000, Number.MAX_SAFE_INTEGER, {}, [1]]) {
    const presentation = getPromptCacheAutoKeepalive({ ...status,
      auto_keepalive_busy_sessions: count as number }, REQUEST_AT, "claude");
    assert.equal(presentation.busySessions, null);
    assert.equal(presentation.connectedSessions, 0);
    assert.equal(presentation.connectionNotice, "감지된 열린 Claude 세션 2개 · 유지 타이머 연결 대기");
  }
  const unknown = getPromptCacheAutoKeepalive({ ...status, auto_keepalive_state: "future_state",
    auto_keepalive_connected_sessions: undefined }, REQUEST_AT, "claude");
  assert.equal(unknown.connectedSessions, null);
  assert.equal(unknown.state, "unknown");
  assert.equal(unknown.connectionNotice, "감지된 열린 Claude 세션 2개 · 유지 타이머 연결 상태 확인 불가");
  const unknownBusy = getPromptCacheAutoKeepalive({ ...status, auto_keepalive_state: "future_state",
    auto_keepalive_busy_sessions: 1 }, REQUEST_AT, "claude");
  assert.equal(unknownBusy.state, "unknown");
  assert.equal(unknownBusy.connectedSessions, 0);
  assert.equal(unknownBusy.connectionNotice, "감지된 열린 Claude 세션 2개 · 자동 유지 상태 확인 필요");
  assert.equal(unknownBusy.nextRequestLabel, null);
  const busy = getPromptCacheAutoKeepalive({ ...status, auto_keepalive_state: "busy" }, REQUEST_AT, "claude");
  assert.equal(busy.connectionNotice, "감지된 열린 Claude 세션 2개 · 응답 완료 후 자동 유지 대기");
  for (const provider of ["codex", undefined] as const) {
    assert.equal(getPromptCacheAutoKeepalive(status, REQUEST_AT, provider).connectionNotice,
      "열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다.");
  }
});

test("registration does not override a disabled, unknown or unsupported automatic-maintenance setting", () => {
  const status = observed({ auto_keepalive_enabled: true, auto_keepalive_supported: true,
    auto_keepalive_state: "busy", auto_keepalive_connected_sessions: 0,
    auto_keepalive_registered_sessions: 2, auto_keepalive_busy_sessions: 1 });
  for (const [overrides, expected] of [
    [{ auto_keepalive_enabled: false }, "자동 유지 꺼짐"],
    [{ auto_keepalive_enabled: undefined }, "자동 유지 설정 확인 불가"],
    [{ auto_keepalive_supported: false }, "현재 경로 미지원"],
    [{ auto_keepalive_supported: undefined }, "자동 유지 지원 확인 필요"],
    [{ auto_keepalive_state: "unsupported" }, "현재 경로 미지원"],
  ] as const) {
    const presentation = getPromptCacheAutoKeepalive({ ...status, ...overrides }, REQUEST_AT, "claude");
    assert.equal(presentation.connectionNotice, `감지된 열린 Claude 세션 2개 · ${expected}`);
    assert.equal(presentation.connectedSessions, 0);
    assert.equal(presentation.nextRequestLabel, null);
  }
});

test("registered sessions retain blocked and pending response labels and safe reasons ahead of busy counts", () => {
  for (const [state, label, reason] of [
    ["human_idle_limit", "유휴 제한으로 중단", "사람 입력 없이 12시간이 지나 자동 요청을 중단했습니다."],
    ["idle", "요청 대기", "사람 입력 없이 12시간이 지나 자동 요청을 중단했습니다."],
    ["cancelled", "유지 요청 취소", "유지 요청이 취소되었습니다. 새 사람 입력이 필요합니다."],
    ["awaiting_response", "유지 응답 대기", "유지 알림을 보냈으며 새 캐시 관측과 응답 완료를 기다립니다."],
    ["pending", "요청 준비 대기", "유지 알림을 보냈으며 새 캐시 관측과 응답 완료를 기다립니다."],
    ["error", "유지 요청 오류", "유지 요청이 취소되었습니다. 새 사람 입력이 필요합니다."],
  ]) {
    const presentation = getPromptCacheAutoKeepalive(observed({ auto_keepalive_enabled: true,
      auto_keepalive_supported: true, auto_keepalive_state: state, auto_keepalive_reason: reason,
      auto_keepalive_connected_sessions: 0, auto_keepalive_registered_sessions: 2,
      auto_keepalive_busy_sessions: 1, auto_keepalive_next_at: REQUEST_AT + 60 }), REQUEST_AT, "claude");
    assert.equal(presentation.connectionNotice, `감지된 열린 Claude 세션 2개 · ${label} · ${reason}`);
    assert.equal(presentation.nextAt, null);
    assert.equal(presentation.nextRequestLabel, null);
    assert.doesNotMatch(presentation.connectionNotice, /응답 완료 후 자동 유지 대기|유지 타이머 연결 대기/);
  }
  const unsafe = getPromptCacheAutoKeepalive(observed({ auto_keepalive_enabled: true,
    auto_keepalive_supported: true, auto_keepalive_state: "error", auto_keepalive_reason: "C:\\private\\session.jsonl",
    auto_keepalive_connected_sessions: 0, auto_keepalive_registered_sessions: 2 }), REQUEST_AT, "claude");
  assert.equal(unsafe.connectionNotice, "감지된 열린 Claude 세션 2개 · 유지 요청 오류");
});

test("Claude explains the real interactive route, visible notification and quota use", () => {
  const presentation = getPromptCacheAutoKeepalive(observed({
    auto_keepalive_enabled: true, auto_keepalive_state: "scheduled",
    auto_keepalive_reason: "claude_official_interactive_plugin_monitor",
  }), REQUEST_AT, "claude");
  assert.match(presentation.tooltip, /공식 인터랙티브 플러그인 모니터/);
  assert.match(presentation.tooltip, /공식 인터랙티브 훅 설치가 확인된 Claude/);
  assert.match(presentation.tooltip, /일반 사용자 입력으로 이미 열린 세션을 등록/);
  assert.match(presentation.tooltip, /해당 입력에 대한 응답 완료를 확인한 뒤 유지 타이머를 연결/);
  assert.match(presentation.tooltip, /세션 선택·재개나 새 세션·재시작은 필요하지 않습니다/);
  assert.match(presentation.tooltip, /같은 작업 세션/);
  assert.match(presentation.tooltip, /cachekeep.*대화 기록.*한도를 소비/);
  assert.match(presentation.tooltip, /미측정 출력은 0으로 표시하지 않/);
  assert.doesNotMatch(presentation.tooltip, /현재 CLI.*미지원|유료 요청을 보내지 않/);
});

test("installed official Claude hooks wait for ordinary input without claiming a connection, cache hit or TTL", () => {
  const status = Object.freeze(observed({ state: "unknown", auto_keepalive_enabled: true,
    auto_keepalive_supported: true, auto_keepalive_reason: "claude_official_live_hooks",
    auto_keepalive_state: "waiting_session", auto_keepalive_connected_sessions: 0,
    observed_at: null, cache_read_tokens: null, cache_write_tokens: null, uncached_input_tokens: null,
    expires_at: null, ttl_seconds: null }));
  const presentation = getPromptCacheAutoKeepalive(status, REQUEST_AT, "claude");
  assert.equal(presentation.connectionNotice, "열린 Claude 세션 등록 대기 · 일반 사용자 입력으로 세션을 등록합니다.");
  assert.equal(presentation.connectedSessions, 0);
  assert.equal(presentation.label, "세션 연결 대기");
  assert.match(presentation.tooltip, /공식 Claude 훅 설정은 실행 중에도 반영됩니다/);
  assert.match(presentation.tooltip, /일반 사용자 입력으로 이미 열린 Claude 세션을 등록/);
  assert.match(presentation.tooltip, /해당 입력에 대한 응답 완료를 확인한 뒤 유지 타이머를 연결/);
  assert.doesNotMatch(presentation.tooltip, /다음.*입력에서.*자동 연결/);
  assert.match(presentation.tooltip, /설치 확인은 실제 연결 수나 캐시 유지 성공의 증거가 아닙니다/);
  assert.doesNotMatch(presentation.tooltip, /claude_official_live_hooks|서버의 유지 경로 사유를 확인할 수 없습니다/);
  assert.equal(presentation.hasUsage, false);
  assert.equal(presentation.outputTokens, null);
  assert.equal(presentation.requests, null);
  assert.equal(presentation.cacheReadTokens, null);
  assert.equal(presentation.nextRequestLabel, null);
  assert.equal(getPromptCacheObservation(status, REQUEST_AT), null);
  assert.equal(getPromptCacheTtl(status, REQUEST_AT), null);
  for (const enabled of [true, false]) {
    const connected = getPromptCacheAutoKeepalive({ ...status, auto_keepalive_enabled: enabled,
      auto_keepalive_connected_sessions: 2 }, REQUEST_AT, "claude");
    assert.equal(connected.connectionNotice, enabled
      ? "연결된 열린 세션 2개에 자동 적용 · 다시 선택할 필요 없음"
      : "연결된 열린 세션 2개 · 옵션을 켜면 자동 적용");
  }
});

test("Claude input registration notice requires supported hook installation and a verified zero connection count", () => {
  const installed = observed({ auto_keepalive_enabled: true, auto_keepalive_supported: true,
    auto_keepalive_reason: "claude_official_live_hooks", auto_keepalive_connected_sessions: 0 });
  const waiting = "열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다.";
  const unproven = [null, undefined, observed(),
    ...[undefined, null, "0", false, -1, 0.5, NaN, Infinity, {}, []].map((count) => ({ ...installed,
      auto_keepalive_connected_sessions: count as number })),
    ...[undefined, null, false, "true", 1].map((supported) => ({ ...installed,
      auto_keepalive_supported: supported as boolean })),
    ...[undefined, null, false, "true", 1].map((enabled) => ({ ...installed,
      auto_keepalive_enabled: enabled as boolean })),
    ...[undefined, "claude_official_interactive_plugin_monitor", "unknown_installation"].map((reason) => ({
      ...installed, auto_keepalive_reason: reason })),
    { ...installed, auto_keepalive_state: "unsupported" },
  ];
  for (const status of unproven) {
    assert.equal(getPromptCacheAutoKeepalive(status, REQUEST_AT, "claude").connectionNotice, waiting);
  }
  for (const provider of ["codex", undefined] as const) {
    assert.equal(getPromptCacheAutoKeepalive(installed, REQUEST_AT, provider).connectionNotice, waiting);
  }
});

test("Codex offers existing history or new managed sessions, supported models and the web limitation", () => {
  const presentation = getPromptCacheAutoKeepalive(observed({
    auto_keepalive_enabled: true, auto_keepalive_state: "waiting_session",
    auto_keepalive_reason: "native_codex_managed_transport",
  }), REQUEST_AT, "codex");
  assert.equal(presentation.label, "세션 연결 대기");
  assert.match(presentation.tooltip, /연결된 열린 관리 CLI 세션에 자동 유지.*다시 선택할 필요가 없/);
  assert.match(presentation.tooltip, /선택한 기존 대화.*관리 CLI 세션.*새 세션.*로컬 캡처/);
  assert.match(presentation.tooltip, /기록과 작업 폴더를 이어 사용/);
  assert.doesNotMatch(presentation.tooltip, /기존 외부 세션.*새 관리 세션을 열어야/);
  assert.match(presentation.tooltip, /기존 앱·작업·전역 설정은 변경하지 않/);
  assert.match(presentation.tooltip, /지원되는 GPT 5\.6\+ 모델/);
  assert.match(presentation.tooltip, /Codex 웹 경로는 지원하지 않/);
  assert.equal(presentation.outputTokens, null);
  const pending = getPromptCacheAutoKeepalive(observed({
    auto_keepalive_enabled: true, auto_keepalive_state: "pending", auto_keepalive_reason: "native_codex_route_pending",
  }), REQUEST_AT, "codex");
  assert.match(pending.tooltip, /유지 요청 경로를 분석 중/);
});

test("current adapter lifecycle states distinguish connecting, waiting, sending and stopped routes", () => {
  for (const [state, label] of [
    ["needs_managed_session", "관리 세션 연결 필요"], ["waiting_for_request", "첫 요청 대기"],
    ["awaiting_prewarm", "예열 지원 확인 대기"], ["prewarming", "예열 요청 중"],
    ["active", "유지 요청 대기"], ["disabled", "자동 유지 중단"], ["stopped", "관리 세션 종료"],
    ["monitor_unavailable", "모니터 연결 대기"], ["awaiting_response", "유지 응답 대기"],
    ["waiting_for_stop", "응답 완료 대기"], ["timer_waiting", "유지 타이머 연결 대기"],
    ["observation_required", "새 캐시 관측 대기"], ["human_input_required", "사용자 입력 대기"],
    ["limit_reached", "자동 요청 한도 도달"], ["daily_budget", "일일 요청 한도 도달"],
    ["quota_unavailable", "사용량·한도 확인 필요"], ["human_idle_limit", "유휴 제한으로 중단"],
    ["account_inactive", "계정 세션 연결 대기"], ["runtime_unavailable", "설정 저장됨 · 연결 확인 필요"],
    ["cancelled", "유지 요청 취소"], ["failed", "Claude 응답 오류 · 복구 대기"], ["ended", "세션 종료"], ["ready", "유지 요청 대기"],
  ]) {
    const presentation = getPromptCacheAutoKeepalive(observed({ auto_keepalive_enabled: true, auto_keepalive_state: state }));
    assert.equal(presentation.label, label);
    assert.equal(presentation.outputTokens, null);
    assert.equal(presentation.cacheReadTokens, null);
  }
  for (const auto_keepalive_state of ["awaiting_prewarm", "active", "ready"]) {
    const presentation = getPromptCacheAutoKeepalive(observed({
      auto_keepalive_enabled: true, auto_keepalive_state, auto_keepalive_next_at: REQUEST_AT + 60,
    }), REQUEST_AT);
    assert.equal(presentation.nextRequestLabel, "다음 요청 1분 0초 후");
  }
});

test("real backend explanations remain specific and zero-output claims require measured zero", () => {
  for (const reason of [
    "관리된 대화형 세션의 모니터 연결이 필요합니다.",
    "새 캐시 유지 CLI 세션을 열어야 합니다. 외부 세션에는 연결하지 않습니다.",
    "새 CLI의 전체 요청을 기다립니다. OAuth prewarm 지원은 아직 확인되지 않았습니다.",
    "전체 요청을 캡처했습니다. OAuth의 출력 없는 prewarm 지원은 첫 완료 응답으로 확인합니다.",
    "유지 알림을 보냈으며 새 캐시 관측과 응답 완료를 기다립니다.",
    "출력 없는 prewarm을 확인하지 못해 이 호스트의 자동 유지를 중단했습니다.",
  ]) assert.ok(getPromptCacheAutoKeepalive(observed({ auto_keepalive_reason: reason })).tooltip.includes(reason));
  const reason = "이 세션의 prewarm 완료와 출력 0을 확인했습니다. 입력 캐시 비용은 발생할 수 있습니다.";
  for (const output of [null, undefined, 3, NaN, -1]) {
    const presentation = getPromptCacheAutoKeepalive(observed({ auto_keepalive_reason: reason, auto_keepalive_output_tokens: output }));
    assert.ok(!presentation.tooltip.includes(reason));
    assert.match(presentation.tooltip, /실제 출력·캐시 읽기 측정값으로 확인/);
  }
  assert.ok(getPromptCacheAutoKeepalive(observed({ auto_keepalive_reason: reason, auto_keepalive_output_tokens: 0 })).tooltip.includes(reason));
});

test("route reasons are translated safely without raw exceptions or blanket CLI limitations", () => {
  for (const [reason, expected] of [
    ["waiting_session", "연결된 관리 작업 세션을 기다립니다."],
    ["session_busy", "대화가 사용 중이어서 유지 요청을 기다립니다."],
    ["waiting_for_stop", "등록된 Claude 세션의 해당 입력에 대한 응답 완료를 기다립니다."],
    ["timer_waiting", "등록된 유휴 Claude 세션의 유지 타이머 연결을 기다립니다."],
    ["codex_web_unsupported", "Codex 웹 경로는 지원하지 않습니다."],
    ["model_unsupported", "무출력 예열은 지원되는 GPT 5.6+ 모델에 한정됩니다."],
    ["다음 유지 요청이 예약되어 있습니다.", "다음 유지 요청이 예약되어 있습니다."],
  ]) assert.ok(getPromptCacheAutoKeepalive(observed({ auto_keepalive_reason: reason })).tooltip.includes(expected));
  for (const reason of [
    "C:\\private\\session.jsonl", "/private/session", "https://example.invalid/private", "<script>private</script>",
    "Error: private exception", "오류: access_token secret", "unknown_route_code", "현재 CLI 미지원으로 설정만 저장합니다.",
    "현재 CLI에서 작업 대화와 동일한 문맥으로 출력 없는 캐시 유지 요청을 보낼 수 없습니다.",
    "zero-output cache-success", "private\nmessage", "x".repeat(241),
  ]) {
    const tooltip = getPromptCacheAutoKeepalive(observed({ auto_keepalive_reason: reason })).tooltip;
    assert.equal(tooltip, `서버의 유지 경로 사유를 확인할 수 없습니다. ${PROMPT_CACHE_AUTO_KEEPALIVE_NOTICE}`);
    assert.ok(!tooltip.includes(reason));
  }
});

test("a scheduled deadline counts down without turning an overdue request into a sent request", () => {
  const status = Object.freeze(observed({
    auto_keepalive_enabled: true, auto_keepalive_state: "scheduled", auto_keepalive_next_at: REQUEST_AT + 60,
  }));
  assert.equal(getPromptCacheAutoKeepalive(status, REQUEST_AT).nextRequestLabel, "다음 요청 1분 0초 후");
  assert.equal(getPromptCacheAutoKeepalive(status, REQUEST_AT + 45).nextRequestLabel, "다음 요청 15초 후");
  for (const now of [REQUEST_AT + 60, REQUEST_AT + 120]) {
    const presentation = getPromptCacheAutoKeepalive(status, now);
    assert.equal(presentation.nextRequestLabel, "다음 요청 · 실행 대기");
    assert.equal(presentation.requests, null);
    assert.equal(presentation.lastAt, null);
    assert.equal(presentation.outputTokens, null);
  }
  for (const state of ["idle", "busy", "pending", "expired", "error", "unsupported"]) {
    assert.equal(getPromptCacheAutoKeepalive({ ...status, auto_keepalive_state: state }, REQUEST_AT).nextRequestLabel, null);
  }
  assert.equal(getPromptCacheTtl(status, REQUEST_AT + 150)?.remainingSeconds, 150);
  assert.equal(status.observed_at, REQUEST_AT);
});

test("keepalive usage remains separate from task telemetry and only measured zero is zero", () => {
  const status = Object.freeze(observed({
    auto_keepalive_enabled: true, auto_keepalive_state: "idle", auto_keepalive_last_at: REQUEST_AT - 10,
    auto_keepalive_requests: 2, auto_keepalive_cache_read_tokens: 8192, auto_keepalive_output_tokens: 3,
  }));
  const presentation = getPromptCacheAutoKeepalive(status, REQUEST_AT);
  assert.equal(presentation.requests, 2);
  assert.equal(presentation.lastAt, REQUEST_AT - 10);
  assert.equal(presentation.cacheReadTokens, 8192);
  assert.equal(presentation.outputTokens, 3);
  assert.equal(presentation.hasUsage, true);
  assert.equal(getPromptCacheObservation(status, REQUEST_AT)?.cacheReadTokens, 600);
  const zero = getPromptCacheAutoKeepalive(observed({ auto_keepalive_cache_read_tokens: 0, auto_keepalive_output_tokens: 0 }));
  assert.equal(zero.outputTokens, 0);
  assert.equal(zero.cacheReadTokens, 0);
  assert.equal(zero.hasUsage, true);
  const unmeasured = getPromptCacheAutoKeepalive(observed({ auto_keepalive_requests: 1 }));
  assert.equal(unmeasured.hasUsage, true);
  assert.equal(unmeasured.cacheReadTokens, null);
  assert.equal(unmeasured.outputTokens, null);
});

test("invalid keepalive counters and timestamps remain unknown", () => {
  for (const value of [null, undefined, -1, 0.5, NaN, Infinity, "3", Number.MAX_SAFE_INTEGER + 1]) {
    const status = observed({
      auto_keepalive_enabled: true, auto_keepalive_state: "scheduled",
      auto_keepalive_requests: value as number, auto_keepalive_cache_read_tokens: value as number,
      auto_keepalive_output_tokens: value as number, auto_keepalive_next_at: value as number,
      auto_keepalive_last_at: value as number,
    });
    const presentation = getPromptCacheAutoKeepalive(status, REQUEST_AT);
    assert.equal(presentation.requests, null);
    assert.equal(presentation.cacheReadTokens, null);
    assert.equal(presentation.outputTokens, null);
    assert.equal(presentation.lastAt, null);
    assert.equal(presentation.nextRequestLabel, "다음 요청 시각 미확인");
  }
  assert.equal(getPromptCacheAutoKeepalive(observed({ auto_keepalive_last_at: REQUEST_AT + 1 }), REQUEST_AT).lastAt, null);
});

async function startFixture(t: TestContext, extraEnv: Record<string, string> = {}) {
  const child = spawn(process.execPath, [fileURLToPath(new URL("./switcherUiFixture.mjs", import.meta.url))], {
    env: { ...process.env, SWITCHER_FIXTURE_PORT: "0", SWITCHER_FIXTURE_KEEPALIVE_ENABLED: "0",
      SWITCHER_FIXTURE_KEEPALIVE_STATE: "", SWITCHER_FIXTURE_KEEPALIVE_MEASURED: "0",
      SWITCHER_FIXTURE_KEEPALIVE_OPEN_ERROR: "0", SWITCHER_FIXTURE_SESSION_LIST_ERROR: "0",
      SWITCHER_FIXTURE_SESSION_LIST_EMPTY: "0", SWITCHER_FIXTURE_SESSION_LIST_TRUNCATED: "0", ...extraEnv },
    stdio: ["ignore", "pipe", "pipe"],
  });
  t.after(() => new Promise<void>((resolve) => {
    if (child.exitCode !== null || child.signalCode !== null) return resolve();
    child.once("exit", () => resolve());
    child.kill();
  }));
  const origin = await new Promise<string>((resolve, reject) => {
    let output = "";
    const timeout = setTimeout(() => reject(new Error("Local fixture did not start")), 5000);
    child.once("error", (error) => { clearTimeout(timeout); reject(error); });
    child.once("exit", () => { clearTimeout(timeout); reject(new Error("Local fixture exited before startup")); });
    child.stdout.on("data", (chunk) => {
      output += String(chunk);
      const match = output.match(/http:\/\/127\.0\.0\.1:\d+/);
      if (match) { clearTimeout(timeout); resolve(match[0]); }
    });
  });
  return {
    async invoke(command: string, args: Record<string, unknown>) {
      const response = await fetch(`${origin}/api/invoke/${command}`, {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(args),
        signal: AbortSignal.timeout(5000),
      });
      return { response, payload: await response.json() };
    },
    async state() {
      return (await fetch(`${origin}/fixture/state`, { signal: AbortSignal.timeout(5000) })).json();
    },
  };
}

test("local fixture scopes preferences and mocks opening a Codex session without cache evidence", async (t) => {
  const fixture = await startFixture(t);
  const codex = { provider: "codex", accountId: "synthetic-codex-alpha" };
  const claude = { provider: "claude", accountId: "11111111-1111-4111-8111-111111111111" };
  const read = async (args: Record<string, unknown>) => (await fixture.invoke("get_prompt_cache_status", args)).payload;
  assert.equal((await read(codex)).auto_keepalive_state, "off");
  const { payload: enabled } = await fixture.invoke("set_prompt_cache_auto_keepalive", { ...codex, enabled: true });
  assert.equal(enabled.auto_keepalive_state, "waiting_session");
  assert.equal((await read({ ...codex, accountId: "synthetic-codex-beta" })).auto_keepalive_enabled, false);
  assert.equal((await read({ ...codex, provider: "claude" })).auto_keepalive_enabled, false);
  const opened = await fixture.invoke("open_prompt_cache_session", codex);
  assert.equal(opened.response.status, 200);
  assert.deepEqual(opened.payload, { started: true });
  assert.deepEqual(await read(codex), enabled);
  const { payload: scheduled } = await fixture.invoke("set_prompt_cache_auto_keepalive", { ...claude, enabled: true });
  assert.equal(scheduled.auto_keepalive_state, "scheduled");
  assert.equal(scheduled.auto_keepalive_requests, 0);
  assert.equal(scheduled.auto_keepalive_cache_read_tokens, null);
  assert.equal(scheduled.auto_keepalive_output_tokens, null);
  assert.deepEqual(await read(claude), scheduled);
  const state = await fixture.state();
  assert.equal(state.calls.filter((call: { command: string }) => call.command === "open_prompt_cache_session").length, 1);
  assert.ok(state.calls.every((call: { command: string }) => [
    "get_prompt_cache_status", "set_prompt_cache_auto_keepalive", "open_prompt_cache_session",
  ].includes(call.command)));
});

test("fixture reports Alpha verified connections and Beta zero without creating connections or cache evidence on opt-in", async (t) => {
  const fixture = await startFixture(t);
  for (const provider of ["claude", "codex"]) {
    for (const [accountId, count] of provider === "claude"
      ? [["11111111-1111-4111-8111-111111111111", 2], ["22222222-2222-4222-8222-222222222222", 0]] as const
      : [["synthetic-codex-alpha", 2], ["synthetic-codex-beta", 0]] as const) {
      const args = { provider, accountId };
      const before = (await fixture.invoke("get_prompt_cache_status", args)).payload;
      assert.equal(before.auto_keepalive_connected_sessions, count);
      assert.equal(before.auto_keepalive_enabled, false);
      const after = (await fixture.invoke("set_prompt_cache_auto_keepalive", { ...args, enabled: true })).payload;
      assert.equal(after.auto_keepalive_connected_sessions, count);
      assert.equal(after.auto_keepalive_enabled, true);
      assert.equal(after.auto_keepalive_requests, 0);
      assert.equal(after.auto_keepalive_cache_read_tokens, null);
      assert.equal(after.auto_keepalive_output_tokens, null);
      assert.deepEqual({ ...after, auto_keepalive_enabled: before.auto_keepalive_enabled,
        auto_keepalive_state: before.auto_keepalive_state, auto_keepalive_next_at: before.auto_keepalive_next_at }, before);
    }
  }
  const unknown = (await fixture.invoke("get_prompt_cache_status", { provider: "claude", accountId: "synthetic-codex-alpha" })).payload;
  assert.equal(unknown.auto_keepalive_connected_sessions, 0);
  assert.ok((await fixture.state()).calls.every((call: BackendCall) => [
    "get_prompt_cache_status", "set_prompt_cache_auto_keepalive",
  ].includes(call.command)));
});

test("local fixture offers measured usage, explicit backend states and launch failure", async (t) => {
  const fixture = await startFixture(t, {
    SWITCHER_FIXTURE_KEEPALIVE_ENABLED: "1", SWITCHER_FIXTURE_KEEPALIVE_STATE: "error",
    SWITCHER_FIXTURE_KEEPALIVE_MEASURED: "1", SWITCHER_FIXTURE_KEEPALIVE_OPEN_ERROR: "1",
  });
  const { payload } = await fixture.invoke("get_prompt_cache_status", {
    provider: "claude", accountId: "11111111-1111-4111-8111-111111111111",
  });
  const presentation = getPromptCacheAutoKeepalive(payload, Date.now() / 1000, "claude");
  assert.equal(presentation.state, "error");
  assert.equal(presentation.requests, 2);
  assert.equal(presentation.cacheReadTokens, 8192);
  assert.equal(presentation.outputTokens, 3);
  const opened = await fixture.invoke("open_prompt_cache_session", { provider: "codex", accountId: "synthetic-codex-alpha" });
  assert.equal(opened.response.status, 500);
  assert.ok(opened.payload.error);
});

test("fixture lists saved Claude and Codex history while off and captures explicit opaque resume IDs", async (t) => {
  const fixture = await startFixture(t);
  for (const args of [
    { provider: "claude", accountId: "11111111-1111-4111-8111-111111111111" },
    { provider: "codex", accountId: "synthetic-codex-alpha" },
  ]) {
    const { payload: list, response } = await fixture.invoke("list_prompt_cache_sessions", args);
    assert.equal(response.status, 200);
    assert.equal(list.sessions.length, 3);
    assert.equal(list.truncated, false);
    assert.equal(list.reason, null);
    const selected = list.sessions.find((session: { resumable: boolean }) => session.resumable);
    assert.match(selected.id, /:saved\/.*\?opaque=1/);
    const off = await fixture.invoke("open_prompt_cache_session", { ...args, sessionId: selected.id });
    assert.equal(off.response.status, 500);
    assert.match(off.payload.error, /자동 유지 옵션을 켜/);
    await fixture.invoke("set_prompt_cache_auto_keepalive", { ...args, enabled: true });
    const before = (await fixture.invoke("get_prompt_cache_status", args)).payload;
    const opened = await fixture.invoke("open_prompt_cache_session", { ...args, sessionId: selected.id });
    assert.deepEqual(opened.payload, { started: true });
    assert.deepEqual((await fixture.invoke("get_prompt_cache_status", args)).payload, before);
    const blocked = list.sessions.find((session: { resumable: boolean }) => !session.resumable);
    assert.equal((await fixture.invoke("open_prompt_cache_session", { ...args, sessionId: blocked.id })).response.status, 500);
    assert.equal((await fixture.invoke("open_prompt_cache_session", { ...args, sessionId: "not-in-this-account" })).response.status, 500);
    const state = await fixture.state();
    assert.ok(state.calls.some((call: Record<string, unknown>) => call.command === "open_prompt_cache_session"
      && call.provider === args.provider && call.accountId === args.accountId && call.sessionId === selected.id));
    const other = (await fixture.invoke("list_prompt_cache_sessions", { ...args,
      accountId: args.provider === "codex" ? "synthetic-codex-beta" : "22222222-2222-4222-8222-222222222222" })).payload;
    assert.ok(other.sessions.every((session: { id: string }) => !list.sessions.some((old: { id: string }) => old.id === session.id)));
  }
});

test("fixture exposes empty, truncated and failed list responses", async (t) => {
  const args = { provider: "codex", accountId: "synthetic-codex-alpha" };
  const empty = await startFixture(t, { SWITCHER_FIXTURE_SESSION_LIST_EMPTY: "1", SWITCHER_FIXTURE_SESSION_LIST_TRUNCATED: "1" });
  assert.deepEqual((await empty.invoke("list_prompt_cache_sessions", args)).payload, { sessions: [], truncated: true, reason: null });
  const failed = await startFixture(t, { SWITCHER_FIXTURE_SESSION_LIST_ERROR: "1" });
  const response = await failed.invoke("list_prompt_cache_sessions", args);
  assert.equal(response.response.status, 500);
  assert.match(response.payload.error, /대화 목록을 읽지 못/);
});

test("resume refusals retain useful explanations without showing raw paths, secrets or unknown codes", () => {
  assert.match(getPromptCacheSessionReason("session_busy")!, /대화가 사용 중/);
  assert.match(getPromptCacheSessionError("auto_keepalive_disabled", "fallback"), /자동 유지 옵션을 켜/);
  const explanation = "이 대화가 사용 중이어서 이어갈 수 없습니다.";
  assert.equal(getPromptCacheSessionError(new Error(explanation), "fallback"), explanation);
  assert.equal(getPromptCacheSessionError(explanation, "fallback"), explanation);
  assert.equal(getPromptCacheSessionReason("This conversation is already in use."), "This conversation is already in use.");
  for (const unsafe of ["C:\\private\\history.jsonl", "https://secret.invalid/token", "<script>failure</script>",
    "access_token secret", "Bearer secret", "failure\nprivate", "new_machine_code", "x".repeat(241), null, {}]) {
    assert.equal(getPromptCacheSessionError(unsafe, "fallback"), "fallback");
  }
});

const componentJs = ts.transpileModule(await readFile(new URL("../src/components/PromptCacheBar.tsx", import.meta.url), "utf8"), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2020 },
}).outputText;
const require = createRequire(import.meta.url);
type BarProps = { provider: PromptCacheProvider; accountId: string };
type BackendCall = { command: string; args: Record<string, unknown> };

// Execute the real component with deterministic hook scheduling and a mock backend.
// This exercises handlers/effect cleanup, not a browser or a live provider route.
async function mountBar(t: TestContext, props: BarProps,
  backend: (command: string, args: Record<string, unknown>) => unknown = () => observed({ auto_keepalive_enabled: true })) {
  const slots: any[] = [];
  const effects = new Map<number, { deps: unknown[]; cleanup?: () => void }>();
  const pendingEffects: (() => void)[] = [];
  const timers = new Map<number, { callback: () => void; ms: number }>();
  const calls: BackendCall[] = [];
  let cursor = 0, dirty = true, tree: any, nextTimer = 0;
  const hooks = {
    useState(initial: any) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = typeof initial === "function" ? initial() : initial;
      return [slots[index], (value: any) => {
        slots[index] = typeof value === "function" ? value(slots[index]) : value;
        dirty = true;
      }];
    },
    useRef(initial: any) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = { current: initial };
      return slots[index];
    },
    useEffect(setup: () => (() => void) | void, deps: unknown[]) {
      const index = cursor++;
      const old = effects.get(index);
      if (old && old.deps.every((value, i) => Object.is(value, deps[i]))) return;
      pendingEffects.push(() => {
        old?.cleanup?.();
        effects.set(index, { deps, cleanup: setup() || undefined });
      });
    },
  };
  const component: any = {};
  new Function("require", "exports", "window", componentJs)(
    (name: string) => name === "react" ? hooks : name === "../lib/promptCache" ? promptCache
      : name === "../lib/platform" ? { invokeBackend: async (command: string, args: Record<string, unknown>) => {
        calls.push({ command, args }); return backend(command, args);
      } } : require(name), component,
    { setInterval(callback: () => void, ms: number) { const id = ++nextTimer; timers.set(id, { callback, ms }); return id; },
      clearInterval(id: number) { timers.delete(id); } },
  );
  function render() {
    cursor = 0; dirty = false;
    tree = component.PromptCacheBar(props);
    for (const effect of pendingEffects.splice(0)) effect();
  }
  async function flush() {
    for (let i = 0; i < 6; i++) {
      if (dirty) render();
      await new Promise<void>((resolve) => setImmediate(resolve));
    }
    if (dirty) render();
  }
  const nodes = (node: any): any[] => Array.isArray(node) ? node.flatMap((child) => nodes(child))
    : node && typeof node === "object" && node.props ? [node, ...nodes(node.props.children)] : [];
  const textOf = (node: any): string => Array.isArray(node) ? node.map(textOf).join("")
    : node && typeof node === "object" && node.props ? textOf(node.props.children)
      : typeof node === "string" || typeof node === "number" ? String(node) : "";
  const button = (text: string) => {
    const found = nodes(tree).find((node) => node.type === "button" && textOf(node) === text);
    assert.ok(found, `Missing button: ${text}`); return found;
  };
  t.after(() => { for (const effect of effects.values()) effect.cleanup?.(); });
  await flush();
  return {
    calls, flush, button,
    text: () => textOf(tree), html: () => renderToStaticMarkup(tree),
    help: () => nodes(tree).find((node) => node.type === "details"),
    checkbox: () => nodes(tree).find((node) => node.type === "input" && node.props.type === "checkbox"),
    radios: () => nodes(tree).filter((node) => node.type === "input" && node.props.type === "radio"),
    async click(label: string) { const target = button(label); assert.ok(!target.props.disabled, `${label} is disabled`); target.props.onClick(); await flush(); },
    async select(id: string) {
      const target = nodes(tree).find((node) => node.type === "input" && node.props.type === "radio" && node.props.value === id);
      assert.ok(target && !target.props.disabled); target.props.onChange(); await flush();
    },
    async setProps(next: BarProps) { props = next; dirty = true; await flush(); },
    async poll() { for (const timer of timers.values()) if (timer.ms === 30_000) timer.callback(); await flush(); },
  };
}

function savedSessions(title = "Saved work"): PromptCacheSessionList {
  return { sessions: [
    { id: "opaque:/saved?key=%2F", title, cwd: "C:/synthetic/project", model: "codex-synthetic",
      updated_at: REQUEST_AT - 120, resumable: true, reason: null, source: "synthetic_history" },
    { id: "blocked-session", title: "Busy saved work", cwd: "C:/synthetic/project", model: null,
      updated_at: REQUEST_AT - 60, resumable: false, reason: "session_busy", source: "synthetic_history" },
  ], truncated: false, reason: null };
}

function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

test("enabling applies to already connected sessions with only preference/status calls and collapsed fallback help", async (t) => {
  for (const provider of ["claude", "codex"] as const) {
    for (const count of [0, 2]) {
      const props = { provider, accountId: "connected-account" };
      let enabled = false;
      const ui = await mountBar(t, props, (command, args) => {
        assert.ok(["get_prompt_cache_status", "set_prompt_cache_auto_keepalive"].includes(command), command);
        if (command === "set_prompt_cache_auto_keepalive") enabled = args.enabled === true;
        return observed({ state: "unknown", auto_keepalive_enabled: enabled, auto_keepalive_connected_sessions: count,
          auto_keepalive_state: "waiting_for_request", observed_at: null, expires_at: null, ttl_seconds: null });
      });
      assert.deepEqual(ui.calls, [{ command: "get_prompt_cache_status", args: props }]);
      assert.equal(ui.help().props.open, undefined);
      assert.equal(ui.help().props.onToggle, undefined);
      assert.equal(ui.help().props.onClick, undefined);
      assert.match(renderToStaticMarkup(ui.help()), /<summary[^>]*>세션 연결 도움말<\/summary>/);
      assert.match(renderToStaticMarkup(ui.help()), /기존 대화 이어가기/);
      assert.match(ui.text(), /연결 어댑터를 지원하지 않는 실행 중인 외부 프로세스에는 자동으로 연결할 수 없/);
      assert.match(ui.text(), /실행 중인 외부 세션에 접속하는 기능은 아닙니다/);
      assert.ok(ui.text().includes(count > 0 ? "연결된 열린 세션 2개 · 옵션을 켜면 자동 적용"
        : "열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다."));
      ui.checkbox().props.onChange({ target: { checked: true } }); await ui.flush();
      assert.deepEqual(ui.calls, [
        { command: "get_prompt_cache_status", args: props },
        { command: "set_prompt_cache_auto_keepalive", args: { ...props, enabled: true } },
      ]);
      assert.ok(ui.text().includes(count > 0 ? "연결된 열린 세션 2개에 자동 적용 · 다시 선택할 필요 없음"
        : "열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다."));
      assert.match(renderToStaticMarkup(ui.help()), /캐시 유지 세션 열기/);
      assert.doesNotMatch(ui.html().replace(renderToStaticMarkup(ui.help()), ""), /<button/);
      assert.match(ui.text(), /프롬프트 캐시 · 관측 없음/);
      assert.doesNotMatch(ui.html(), /role="progressbar"|유지 요청 \d+회/);
      await ui.poll();
      assert.deepEqual(ui.calls.at(-1), { command: "get_prompt_cache_status", args: props });
      assert.equal(ui.calls.length, 3);
    }
  }
});

test("component shows missing, malformed and unsupported connection evidence honestly", async (t) => {
  for (const count of [undefined, "2", -1, 1.5, NaN, Infinity, {}, 0]) {
    const ui = await mountBar(t, { provider: "codex", accountId: "unknown-connections" }, () => observed({
      auto_keepalive_enabled: true, auto_keepalive_connected_sessions: count as number,
      auto_keepalive_supported: false, auto_keepalive_state: "unsupported", auto_keepalive_reason: "codex_web_unsupported",
    }));
    assert.match(ui.text(), /열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다\./);
    assert.match(ui.text(), /현재 경로 미지원/);
    assert.doesNotMatch(ui.text(), /연결된 열린 세션 \d+개에 자동 적용|유지 요청 \d+회/);
    assert.equal(ui.calls.length, 1);
  }
  const missing = await mountBar(t, { provider: "claude", accountId: "unknown-preference" }, () => observed({
    auto_keepalive_connected_sessions: 2, auto_keepalive_enabled: "true" as unknown as boolean,
  }));
  assert.match(missing.text(), /연결된 열린 세션 2개 · 자동 유지 설정 확인 불가/);
  assert.equal(missing.checkbox().props.disabled, true);
  assert.equal(missing.checkbox().props.checked, false);
});

test("Claude live-hook opt-in waits for natural input and preserves native external-session help without automatic actions", async (t) => {
  const props: BarProps = { provider: "claude", accountId: "installed-hooks" };
  let enabled = false, connected = 0;
  const ui = await mountBar(t, props, (command, args) => {
    assert.ok(["get_prompt_cache_status", "set_prompt_cache_auto_keepalive"].includes(command), command);
    if (command === "set_prompt_cache_auto_keepalive") enabled = args.enabled === true;
    return observed({ state: "unknown", auto_keepalive_enabled: enabled, auto_keepalive_connected_sessions: connected,
      auto_keepalive_supported: true, auto_keepalive_reason: "claude_official_live_hooks",
      auto_keepalive_state: "waiting_session", observed_at: null, expires_at: null, ttl_seconds: null });
  });
  assert.match(ui.text(), /열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다/);
  ui.checkbox().props.onChange({ target: { checked: true } }); await ui.flush();
  assert.match(ui.text(), /열린 Claude 세션 등록 대기 · 일반 사용자 입력으로 세션을 등록합니다\./);
  assert.match(ui.html(), /세션 선택·재개나 새 세션·재시작은 필요하지 않습니다/);
  assert.doesNotMatch(ui.html(), /연결된 열린 세션 \d+개에 자동 적용|role="progressbar"|유지 요청 \d+회/);
  assert.equal(ui.help().props.open, undefined);
  assert.deepEqual(ui.calls, [
    { command: "get_prompt_cache_status", args: props },
    { command: "set_prompt_cache_auto_keepalive", args: { ...props, enabled: true } },
  ]);
  // Only a later backend observation can establish actual living connections.
  connected = 2; await ui.poll();
  assert.match(ui.text(), /연결된 열린 세션 2개에 자동 적용 · 다시 선택할 필요 없음/);
  assert.doesNotMatch(ui.text(), /일반 사용자 입력으로 세션을 등록합니다/);
  connected = 0; await ui.setProps({ provider: "codex", accountId: props.accountId });
  assert.match(ui.text(), /열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다/);
  assert.doesNotMatch(ui.text(), /열린 Claude 세션 등록 대기/);
  assert.match(ui.text(), /연결 어댑터를 지원하지 않는 실행 중인 외부 프로세스에는 자동으로 연결할 수 없/);
  assert.ok(ui.calls.every((call) => ["get_prompt_cache_status", "set_prompt_cache_auto_keepalive"].includes(call.command)));
});

test("Claude component renders registered busy and timer waits before a real connection with status polling only", async (t) => {
  const props: BarProps = { provider: "claude", accountId: "registered-hooks" };
  let state = "busy", busy = 1, connected = 0;
  const ui = await mountBar(t, props, (command) => {
    assert.equal(command, "get_prompt_cache_status");
    return observed({ state: "unknown", auto_keepalive_enabled: true, auto_keepalive_supported: true,
      auto_keepalive_reason: "claude_official_live_hooks", auto_keepalive_state: state,
      auto_keepalive_registered_sessions: 2, auto_keepalive_busy_sessions: busy,
      auto_keepalive_connected_sessions: connected, auto_keepalive_next_at: REQUEST_AT + 60,
      observed_at: null, expires_at: null, ttl_seconds: null });
  });
  assert.match(ui.text(), /감지된 열린 Claude 세션 2개 · 작업 중 1개 · 응답 완료 후 자동 유지 대기/);
  assert.match(ui.text(), /대화 사용 중/);
  assert.doesNotMatch(ui.text(), /등록 대기|다음.*입력|연결된 열린 세션 \d+개|다음 요청/);
  state = "waiting_for_stop"; busy = 0; await ui.poll();
  assert.match(ui.text(), /감지된 열린 Claude 세션 2개 · 응답 완료 후 자동 유지 대기/);
  assert.match(ui.text(), /응답 완료 대기/);
  assert.doesNotMatch(ui.text(), /등록 대기|다음.*입력|다음 요청/);
  state = "timer_waiting"; await ui.poll();
  assert.match(ui.text(), /감지된 열린 Claude 세션 2개 · 유지 타이머 연결 대기/);
  assert.doesNotMatch(ui.text(), /등록 대기|다음.*입력|연결된 열린 세션 \d+개|다음 요청/);
  connected = 2; await ui.poll();
  assert.match(ui.text(), /연결된 열린 세션 2개에 자동 적용 · 다시 선택할 필요 없음/);
  assert.doesNotMatch(ui.text(), /감지된 열린 Claude 세션|다음.*입력|다음 요청/);
  assert.deepEqual(ui.calls, Array.from({ length: 4 }, () => ({ command: "get_prompt_cache_status", args: props })));
});

test("preference write guards discard an older connection-count poll and serialize duplicate enabling", async (t) => {
  const oldPoll = deferred<PromptCacheStatus>(), write = deferred<PromptCacheStatus>();
  let reads = 0;
  const ui = await mountBar(t, { provider: "codex", accountId: "connected" }, (command) => {
    if (command === "get_prompt_cache_status" && ++reads === 2) return oldPoll.promise;
    if (command === "set_prompt_cache_auto_keepalive") return write.promise;
    return observed({ auto_keepalive_enabled: false, auto_keepalive_connected_sessions: 2 });
  });
  await ui.poll();
  const checkbox = ui.checkbox();
  checkbox.props.onChange({ target: { checked: true } });
  checkbox.props.onChange({ target: { checked: true } });
  await ui.flush(); await ui.poll();
  assert.equal(ui.checkbox().props.disabled, true);
  assert.equal(ui.calls.filter((call) => call.command === "set_prompt_cache_auto_keepalive").length, 1);
  assert.equal(reads, 2);
  write.resolve(observed({ auto_keepalive_enabled: true, auto_keepalive_connected_sessions: 2 })); await ui.flush();
  oldPoll.resolve(observed({ auto_keepalive_enabled: false, auto_keepalive_connected_sessions: 9 })); await ui.flush();
  assert.equal(ui.checkbox().props.checked, true);
  assert.match(ui.text(), /연결된 열린 세션 2개에 자동 적용/);
  assert.doesNotMatch(ui.text(), /연결된 열린 세션 9개/);
  assert.ok(ui.calls.every((call) => ["get_prompt_cache_status", "set_prompt_cache_auto_keepalive"].includes(call.command)));
});

test("late status/preference replies and failures cannot restore another account's connected sessions", async (t) => {
  for (const next of [{ provider: "codex", accountId: "other" }, { provider: "claude", accountId: "previous" }] as BarProps[]) {
    for (const action of ["get_prompt_cache_status", "set_prompt_cache_auto_keepalive"]) {
      for (const fail of [false, true]) {
        const initial: BarProps = { provider: "codex", accountId: "previous" };
        const old = deferred<PromptCacheStatus>();
        let pending = true;
        const ui = await mountBar(t, initial, (command, args) => {
          if (pending && command === action && args.provider === initial.provider && args.accountId === initial.accountId) {
            pending = false; return old.promise;
          }
          return observed({ auto_keepalive_enabled: false, auto_keepalive_connected_sessions: 0 });
        });
        if (action === "set_prompt_cache_auto_keepalive") {
          ui.checkbox().props.onChange({ target: { checked: true } }); await ui.flush();
        }
        const previousHelp = ui.help();
        await ui.setProps(next);
        assert.notEqual(ui.help().key, previousHelp.key);
        assert.match(ui.text(), /열린 세션 연결 대기/);
        await ui.setProps(initial);
        if (fail) old.reject(new Error("Old account failure"));
        else old.resolve(observed({ auto_keepalive_enabled: true, auto_keepalive_connected_sessions: 9 }));
        await ui.flush();
        assert.match(ui.text(), /열린 세션 연결 대기/);
        assert.doesNotMatch(ui.text(), /연결된 열린 세션 9개|Old account failure|설정을 저장하지 못/);
        assert.equal(ui.checkbox().props.checked, false);
        assert.ok(ui.calls.every((call) => ["get_prompt_cache_status", "set_prompt_cache_auto_keepalive"].includes(call.command)));
      }
    }
  }
});

test("component lists while off, escapes titles, requires explicit resumable selection and preserves new-session args", async (t) => {
  const props: BarProps = { provider: "codex", accountId: "fixture-account" };
  const list = savedSessions('<script>alert("saved")</script>');
  let enabled = false;
  const ui = await mountBar(t, props, (command, args) => {
    if (command === "list_prompt_cache_sessions") return list;
    if (command === "set_prompt_cache_auto_keepalive") enabled = args.enabled === true;
    return command === "open_prompt_cache_session" ? { started: true } : observed({ auto_keepalive_enabled: enabled });
  });
  assert.equal(ui.calls.filter((call) => call.command === "list_prompt_cache_sessions").length, 0);
  await ui.click("기존 대화 이어가기");
  assert.ok(ui.radios().every((radio) => !radio.props.checked));
  assert.equal(ui.radios()[1].props.disabled, true);
  ui.radios()[1].props.onChange(); await ui.flush();
  assert.ok(ui.radios().every((radio) => !radio.props.checked));
  assert.ok(ui.button("선택한 대화 이어가기").props.disabled);
  assert.match(ui.html(), /&lt;script&gt;/);
  assert.doesNotMatch(ui.html(), /<script[ >]/);
  assert.match(ui.text(), /C:\/synthetic\/project/);
  assert.match(ui.text(), /대화가 사용 중/);
  await ui.select(list.sessions[0].id);
  assert.ok(ui.button("선택한 대화 이어가기").props.disabled);
  ui.button("선택한 대화 이어가기").props.onClick(); await ui.flush();
  assert.equal(ui.calls.filter((call) => call.command === "open_prompt_cache_session").length, 0);
  ui.checkbox().props.onChange({ target: { checked: true } }); await ui.flush();
  await ui.click("선택한 대화 이어가기");
  assert.deepEqual(ui.calls.find((call) => call.command === "open_prompt_cache_session")?.args,
    { ...props, sessionId: list.sessions[0].id });
  await ui.click("캐시 유지 세션 열기");
  assert.deepEqual(ui.calls.filter((call) => call.command === "open_prompt_cache_session").at(-1)?.args, props);
  await ui.click("목록 새로고침");
  assert.ok(ui.radios().every((radio) => !radio.props.checked));
  assert.ok(ui.button("선택한 대화 이어가기").props.disabled);
  ui.button("선택한 대화 이어가기").props.onClick(); await ui.flush();
  assert.equal(ui.calls.filter((call) => call.command === "open_prompt_cache_session").length, 2);
});

test("component serializes list/open/preferences and invalidates older status polls", async (t) => {
  const pendingStatus = deferred<PromptCacheStatus>(), pendingList = deferred<PromptCacheSessionList>(), pendingOpen = deferred<unknown>();
  let statusReads = 0;
  const ui = await mountBar(t, { provider: "claude", accountId: "same-account" }, (command) => {
    if (command === "get_prompt_cache_status" && ++statusReads === 2) return pendingStatus.promise;
    if (command === "list_prompt_cache_sessions") return pendingList.promise;
    if (command === "open_prompt_cache_session") return pendingOpen.promise;
    return observed({ auto_keepalive_enabled: true });
  });
  await ui.poll();
  const listButton = ui.button("기존 대화 이어가기");
  await ui.click("기존 대화 이어가기");
  listButton.props.onClick();
  ui.checkbox().props.onChange({ target: { checked: false } });
  await ui.poll();
  assert.equal(ui.calls.filter((call) => call.command === "list_prompt_cache_sessions").length, 1);
  assert.equal(ui.calls.filter((call) => call.command === "set_prompt_cache_auto_keepalive").length, 0);
  assert.equal(statusReads, 2);
  pendingStatus.resolve(observed({ auto_keepalive_enabled: false })); await ui.flush();
  pendingList.resolve(savedSessions()); await ui.flush();
  assert.equal(ui.checkbox().props.checked, true);
  await ui.select(savedSessions().sessions[0].id);
  const resume = ui.button("선택한 대화 이어가기");
  await ui.click("선택한 대화 이어가기");
  resume.props.onClick(); await ui.poll();
  assert.equal(ui.calls.filter((call) => call.command === "open_prompt_cache_session").length, 1);
  assert.equal(statusReads, 2);
  pendingOpen.resolve({ started: true }); await ui.flush();
  assert.equal(statusReads, 3);
  assert.equal(ui.checkbox().props.checked, true);
});

test("late list replies and failures cannot leak across account/provider changes or a return to the same account", async (t) => {
  for (const next of [{ provider: "codex", accountId: "other-account" }, { provider: "claude", accountId: "same-account" }] as BarProps[]) {
    for (const fail of [false, true]) {
      const initial: BarProps = { provider: "codex", accountId: "same-account" };
      const old = deferred<PromptCacheSessionList>();
      let firstList = true;
      const ui = await mountBar(t, initial, (command) => {
        if (command === "list_prompt_cache_sessions") {
          if (firstList) { firstList = false; return old.promise; }
          return savedSessions("Current scope history");
        }
        return observed({ auto_keepalive_enabled: true });
      });
      await ui.click("기존 대화 이어가기");
      await ui.setProps(next);
      assert.equal(ui.radios().length, 0);
      await ui.setProps(initial);
      await ui.click("기존 대화 이어가기");
      if (fail) old.reject("Private old account refusal"); else old.resolve(savedSessions("Private old account history"));
      await ui.flush();
      assert.match(ui.text(), /Current scope history/);
      assert.doesNotMatch(ui.text(), /Private old account/);
      assert.ok(ui.radios().every((radio) => !radio.props.checked));
      assert.ok(ui.button("선택한 대화 이어가기").props.disabled);
    }
  }
});

test("late resume results never refresh or show a refusal for the next provider/account", async (t) => {
  for (const fail of [false, true]) {
    const old = deferred<unknown>();
    const initial: BarProps = { provider: "claude", accountId: "previous" };
    const ui = await mountBar(t, initial, (command) => command === "list_prompt_cache_sessions" ? savedSessions()
      : command === "open_prompt_cache_session" ? old.promise : observed({ auto_keepalive_enabled: true }));
    await ui.click("기존 대화 이어가기");
    await ui.select(savedSessions().sessions[0].id);
    await ui.click("선택한 대화 이어가기");
    await ui.setProps({ provider: "codex", accountId: "next" });
    if (fail) old.reject("Previous account refusal"); else old.resolve({ started: true });
    await ui.flush();
    assert.doesNotMatch(ui.text(), /Previous account refusal|Saved work/);
    assert.equal(ui.calls.filter((call) => call.command === "get_prompt_cache_status" && call.args.accountId === initial.accountId).length, 1);
  }
});

test("component shows empty/truncated lists, retries list failures and keeps resume refusals after a status poll", async (t) => {
  let listRequests = 0;
  const ui = await mountBar(t, { provider: "codex", accountId: "fixture" }, (command) => {
    if (command === "list_prompt_cache_sessions") {
      if (++listRequests === 1) throw new Error("저장된 대화 목록을 읽지 못했습니다.");
      if (listRequests === 2) return { sessions: [], truncated: true, reason: "일부 기록을 읽지 못했습니다." };
      return savedSessions();
    }
    if (command === "open_prompt_cache_session") throw "session_busy";
    return observed({ auto_keepalive_enabled: true });
  });
  await ui.click("기존 대화 이어가기");
  assert.match(ui.text(), /대화 목록을 읽지 못/);
  await ui.poll();
  assert.match(ui.text(), /대화 목록을 읽지 못/);
  await ui.click("목록 새로고침");
  assert.match(ui.text(), /저장된 대화가 없습니다/);
  assert.match(ui.text(), /일부 저장된 대화만 표시/);
  assert.match(ui.text(), /일부 기록을 읽지 못/);
  assert.ok(ui.button("선택한 대화 이어가기").props.disabled);
  await ui.click("목록 새로고침");
  await ui.select(savedSessions().sessions[0].id);
  await ui.click("선택한 대화 이어가기");
  assert.match(ui.text(), /대화가 사용 중/);
  assert.doesNotMatch(ui.text(), /session_busy/);
  await ui.poll();
  assert.match(ui.text(), /대화가 사용 중/);
});

function observed(overrides: Partial<PromptCacheStatus> = {}): PromptCacheStatus {
  return {
    state: "observed",
    observed_at: REQUEST_AT,
    model: "claude-sonnet-synthetic",
    session_id: "01234567-89ab-cdef-0123-456789abcdef",
    cache_read_tokens: 600,
    cache_write_tokens: 200,
    uncached_input_tokens: 200,
    expires_at: REQUEST_AT + 300,
    ttl_seconds: 300,
    ttl_source: "synthetic estimate",
    reason: null,
    ...overrides,
  };
}

test("TTL is remaining time from the actual request, separately from the last-request hit rate", () => {
  const status = observed();
  assert.deepEqual(getPromptCacheTtl(status, REQUEST_AT + 60), {
    expiresAt: REQUEST_AT + 300,
    remainingSeconds: 240,
    remainingPercent: 80,
  });
  assert.equal(getPromptCacheObservation(status, REQUEST_AT + 60)?.hitPercent, 60);
});

test("polling an unchanged observation never restarts its TTL or mutates the response", () => {
  const status = Object.freeze(observed());
  const original = structuredClone(status);
  assert.equal(getPromptCacheTtl(status, REQUEST_AT)?.remainingPercent, 100);
  assert.equal(getPromptCacheTtl({ ...status }, REQUEST_AT + 150)?.remainingPercent, 50);
  assert.equal(getPromptCacheTtl({ ...status }, REQUEST_AT + 290)?.remainingSeconds, 10);
  assert.deepEqual(status, original);
});

test("known expiry clamps to zero at and after expiry without erasing historical cache hits", () => {
  for (const now of [REQUEST_AT + 300, REQUEST_AT + 301, REQUEST_AT + 1_000_000]) {
    assert.deepEqual(getPromptCacheTtl(observed(), now), {
      expiresAt: REQUEST_AT + 300, remainingSeconds: 0, remainingPercent: 0,
    });
    assert.equal(getPromptCacheObservation(observed(), now)?.cacheReadTokens, 600);
    assert.equal(getPromptCacheObservation(observed(), now)?.hitPercent, 60);
  }
});

test("backend expiry beyond the request TTL is capped and an earlier expiry is respected", () => {
  assert.deepEqual(getPromptCacheTtl(observed({ expires_at: REQUEST_AT + 900 }), REQUEST_AT), {
    expiresAt: REQUEST_AT + 300, remainingSeconds: 300, remainingPercent: 100,
  });
  assert.deepEqual(getPromptCacheTtl(observed({ expires_at: REQUEST_AT + 120 }), REQUEST_AT + 60), {
    expiresAt: REQUEST_AT + 120, remainingSeconds: 60, remainingPercent: 20,
  });
});

test("future, invalid, and millisecond request timestamps cannot create observations or TTL", () => {
  for (const observed_at of [0, -1, NaN, Infinity, -Infinity, REQUEST_AT + 1, REQUEST_AT * 1000, "1800000000"]) {
    const status = observed({ observed_at: observed_at as number | null });
    assert.equal(getPromptCacheObservation(status, REQUEST_AT), null, String(observed_at));
    assert.equal(getPromptCacheTtl(status, REQUEST_AT), null, String(observed_at));
  }
});

test("older Claude token telemetry without a request timestamp still shows hits, never a TTL countdown", () => {
  for (const observed_at of [null, undefined]) {
    const status = observed({observed_at:observed_at as number|null});
    assert.equal(getPromptCacheObservation(status, REQUEST_AT)?.hitPercent, 60);
    assert.equal(getPromptCacheObservation(status, REQUEST_AT)?.observedAt, null);
    assert.equal(getPromptCacheTtl(status, REQUEST_AT), null);
  }
});

test("invalid current time and missing or unknown backend status stay unknown", () => {
  for (const now of [0, -1, NaN, Infinity, -Infinity]) {
    assert.equal(getPromptCacheObservation(observed(), now), null);
    assert.equal(getPromptCacheTtl(observed(), now), null);
  }
  for (const status of [null, undefined, observed({ state: "unknown" })]) {
    assert.equal(getPromptCacheObservation(status, REQUEST_AT), null);
    assert.equal(getPromptCacheTtl(status, REQUEST_AT), null);
  }
});

test("missing, non-positive, and non-finite TTL or expiry never produce a fake progress value", () => {
  for (const field of ["ttl_seconds", "expires_at"] as const) {
    for (const value of [null, undefined, 0, -1, NaN, Infinity, -Infinity, "300"]) {
      assert.equal(getPromptCacheTtl(observed({ [field]: value }), REQUEST_AT), null, `${field}: ${value}`);
    }
  }
  for (const expires_at of [REQUEST_AT, REQUEST_AT - 1]) {
    assert.equal(getPromptCacheTtl(observed({ expires_at }), REQUEST_AT), null);
  }
  assert.equal(getPromptCacheTtl(observed({ observed_at: 1e308, ttl_seconds: 1e308, expires_at: 1.5e308 }), 1e308), null);
});

test("known TTL without positive actual cache reads or writes remains unknown", () => {
  for (const value of [null, undefined, 0, -1, 0.5, NaN, Infinity, "600", Number.MAX_SAFE_INTEGER + 1]) {
    const status = observed({ cache_read_tokens: value as number | null, cache_write_tokens: value as number | null });
    assert.equal(getPromptCacheTtl(status, REQUEST_AT), null, String(value));
  }
});

test("an actual cache write also supports a TTL estimate, but a write is not a read hit", () => {
  const status = observed({ cache_read_tokens: 0, cache_write_tokens: 800 });
  assert.equal(getPromptCacheTtl(status, REQUEST_AT + 150)?.remainingPercent, 50);
  assert.equal(getPromptCacheObservation(status, REQUEST_AT)?.hitPercent, 0);
});

test("a positive cache observation can support TTL even when other counters are missing", () => {
  for (const status of [
    observed({ cache_read_tokens: null }),
    observed({ cache_write_tokens: null }),
    observed({ uncached_input_tokens: null }),
  ]) {
    assert.equal(getPromptCacheTtl(status, REQUEST_AT + 150)?.remainingPercent, 50);
    assert.equal(getPromptCacheObservation(status, REQUEST_AT)?.hitPercent, null);
  }
});

test("Codex web or unattributable no-TTL data preserves observations without inventing expiry", () => {
  const status = observed({ model: "gpt-synthetic", ttl_seconds: null, expires_at: null, ttl_source: null });
  assert.equal(getPromptCacheTtl(status, REQUEST_AT), null);
  assert.deepEqual(getPromptCacheObservation(status, REQUEST_AT), {
    observedAt: REQUEST_AT, cacheReadTokens: 600, cacheWriteTokens: 200,
    uncachedInputTokens: 200, hitPercent: 60,
  });
  assert.equal(getPromptCacheTtl(observed({ state: "unknown", reason: "unattributable" }), REQUEST_AT), null);
});

test("invalid and missing token counters stay unknown rather than being treated as zero", () => {
  for (const field of ["cache_read_tokens", "cache_write_tokens", "uncached_input_tokens"] as const) {
    for (const value of [null, undefined, -1, 0.5, NaN, Infinity, -Infinity, "123", Number.MAX_SAFE_INTEGER + 1]) {
      assert.equal(getPromptCacheObservation(observed({ [field]: value }), REQUEST_AT)?.hitPercent, null);
    }
  }
});

test("zero input has no hit percentage; real misses and full read hits are valid measurements", () => {
  assert.equal(getPromptCacheObservation(observed({ cache_read_tokens: 0, cache_write_tokens: 0, uncached_input_tokens: 0 }), REQUEST_AT)?.hitPercent, null);
  assert.equal(getPromptCacheObservation(observed({ cache_read_tokens: 0, cache_write_tokens: 0 }), REQUEST_AT)?.hitPercent, 0);
  assert.equal(getPromptCacheObservation(observed({ cache_write_tokens: 0, uncached_input_tokens: 0 }), REQUEST_AT)?.hitPercent, 100);
  assert.equal(getPromptCacheObservation(observed({ cache_read_tokens: Number.MAX_SAFE_INTEGER }), REQUEST_AT)?.hitPercent, null);
});

test("model and session scope uses a concise masked ID, including short session IDs", () => {
  assert.equal(formatPromptCacheScope("gpt-synthetic", "01234567-89ab-cdef-0123-456789abcdef"), "모델 gpt-synthetic · 세션 0123…cdef");
  assert.equal(formatPromptCacheScope("claude-synthetic", "abcdefghi"), "모델 claude-synthetic · 세션 ab…hi");
  assert.equal(formatPromptCacheScope("claude-synthetic", "tiny"), "모델 claude-synthetic · 세션 ••••");
  assert.equal(formatPromptCacheScope(null, null), "모델 확인 불가 · 세션 확인 불가");
});

test("scope never exposes paths, URLs, markup, control characters, or unbounded identifiers", () => {
  for (const unsafe of [
    "C:\\Users\\private\\sessions\\secret.jsonl", "/home/private/session", "../private/session",
    "file:///private/session", "https://example.invalid/private/session", "\\\\host\\private\\session",
    "<script>alert(1)</script>", "session\nprivate", "session\n", "session\r\n", "session\t",
    "secret%2Fprivate", "x".repeat(129),
  ]) {
    assert.equal(formatPromptCacheScope(unsafe, unsafe), "모델 확인 불가 · 세션 확인 불가");
  }
});

test("estimated duration formatting handles boundaries without manufacturing invalid durations", () => {
  for (const [seconds, expected] of [[0, "0초"], [0.5, "1초"], [59, "59초"], [60, "1분 0초"], [299, "4분 59초"], [3600, "1시간 0분"]] as const) {
    assert.equal(formatPromptCacheDuration(seconds), expected);
  }
  for (const seconds of [-1, NaN, Infinity]) assert.equal(formatPromptCacheDuration(seconds), "확인 불가");
});

test("warmup guidance explicitly explains that a separate context does not extend the task cache", () => {
  assert.equal(PROMPT_CACHE_WARMUP_NOTICE, "워밍업은 별도 문맥으로 실행되며 작업 캐시를 연장하지 않습니다.");
});

test("unknown-status tooltips preserve safe backend explanations including Codex account TTL limitations", () => {
  for (const reason of [
    "Codex web does not expose account-attributable cache TTL.",
    "Codex CLI/web does not expose account-attributable cache TTL.",
    "계정에 귀속할 수 있는 실제 요청의 캐시 관측값이 없습니다.",
    "캐시 토큰은 관측되었지만 만료시간은 제공되지 않습니다.",
  ]) assert.equal(getPromptCacheReason(`  ${reason}  `), reason);
});

test("unknown-status tooltips omit absent, unsafe, or oversized reasons instead of exposing raw details", () => {
  for (const reason of [
    null, undefined, "", "  ", "C:\\Users\\private\\session.jsonl", "/home/private/session",
    "error at ../private/session", "https://example.invalid/private", "<script>private</script>",
    "message\nprivate", "message\u0000private", "x".repeat(241),
  ]) assert.equal(getPromptCacheReason(reason), undefined);
});
