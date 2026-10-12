import assert from "node:assert/strict";
import test from "node:test";
import { formatClaudeRemoteControlStatus, type ClaudeRemoteControlStatus } from "../src/lib/claude.ts";

const NOW = 1_800_000_000;
function status(patch: Partial<ClaudeRemoteControlStatus> = {}): ClaudeRemoteControlStatus {
  return { checked_at: NOW, registered: 1, connected: 0, requested: 0, waiting: 1,
    runtime_bound: 1, transport: "synthetic", error: null, ...patch };
}

const reasonCases: [string, RegExp][] = [
  ["busy", /응답·작업 진행 중.*응답이 완료된 뒤/],
  ["shell", /셸 화면.*정상 Claude 입력창으로 돌아올 때까지/],
  ["permission_required", /권한·사용자 확인.*확인 화면이 있으면.*직접 처리/],
  ["response_failed", /응답이 실패.*오류를 확인.*정상 입력창/],
  ["waiting_for_empty_prompt", /빈 Claude 입력창 확인 대기.*작성 중인 입력은 유지/],
  ["waiting_for_stable_prompt", /입력창이 안정될 때까지/],
  ["waiting_for_disconnect_evidence", /이전 계정.*연결 해제를 아직 확인하지 못/],
  ["waiting_for_auth_transition", /계정 변경 반영.*새 계정 연결은 아직 확인하지 못/],
  ["initial_activation_required", /최초 RC 활성화·동의.*이용 자격.*직접 확인/],
  ["awaiting_connection_confirmation", /RC 연결 결과 확인을 기다/],
  ["unavailable_or_changed", /접근할 수 없거나 실행 정보가 변경.*연결을 확인할 수 없/],
  ["policy_restricted", /조직·프로젝트 정책.*이용 자격/],
  ["connection_unverified", /연결을 아직 확인하지 못.*런타임 등록만으로 연결 완료를 판단하지/],
];

for (const [reason, expected] of reasonCases) {
  test(`RC explains ${reason} without promoting a binding or input to connection proof`, () => {
    const view = formatClaudeRemoteControlStatus(status({ waiting_reasons: { [reason]: 2 } }), NOW);
    assert.equal(view.details.length, 1);
    assert.match(view.details[0], expected);
    assert.match(view.details[0], /\(2개\)$/);
    assert.match(view.summary, /연결 확인 0.*요청 후 확인 대기 0.*런타임 연결 등록 1 \(접속 확인 별도\)/);
    assert.doesNotMatch(view.details.join(" "), /다음 입력.*연결|입력하면.*연결|연결되었습니다|연결 완료되었습니다/);
    if (reason === "waiting_for_empty_prompt") assert.doesNotMatch(view.details[0], /입력이 있습니다|초안을 감지|작성 중인 입력을 감지/);
    if (reason === "permission_required") assert.doesNotMatch(view.details[0], /RC 동의|최초 동의|직접 승인/);
    assert.equal(view.retryNotice, null);
  });
}

test("RC preserves overlapping counts and multiple independent waiting reasons", () => {
  const original = status({ registered: 5, connected: 2, requested: 1, waiting: 3, runtime_bound: 4,
    waiting_reasons: { busy: 2, permission_required: 1 }, retry_at: NOW + 9 });
  const before = structuredClone(original);
  const view = formatClaudeRemoteControlStatus(original, NOW);
  assert.match(view.summary, /실행 세션 5 · 연결 확인 2 · 요청 후 확인 대기 1 · 대기\/보류 3 · 런타임 연결 등록 4/);
  assert.equal(view.details.length, 2);
  assert.match(view.details[0], /응답이 완료된 뒤.*\(2개\)/);
  assert.match(view.details[1], /직접 처리.*\(1개\)/);
  assert.deepEqual(original, before);
});

test("RC remains compatible with snapshots lacking optional reasons, binding and timer", () => {
  const view = formatClaudeRemoteControlStatus(status({ runtime_bound: undefined }), NOW);
  assert.match(view.summary, /연결 확인 0.*대기\/보류 1/);
  assert.doesNotMatch(view.summary, /런타임 연결 등록/);
  assert.deepEqual(view.details, ["대기 사유를 아직 확인하지 못했습니다."]);
  assert.equal(view.retryNotice, null);
});

test("missing RC telemetry and unstarted monitor do not claim an active search or connection", () => {
  for (const missing of [undefined, null]) {
    const view = formatClaudeRemoteControlStatus(missing, NOW);
    assert.equal(view.summary, "리모트 컨트롤: 상태 확인 대기");
    assert.deepEqual(view.details, ["RC 상태 정보를 아직 받지 못했습니다."]);
    assert.equal(view.retryNotice, null);
  }
  const view = formatClaudeRemoteControlStatus(status({ checked_at: 0, registered: 0, waiting: 0, retry_at: NOW + 5 }), NOW);
  assert.deepEqual(view.details, ["RC 모니터의 첫 상태 확인을 기다립니다."]);
  assert.equal(view.retryNotice, null);
  assert.doesNotMatch(view.details.join(" "), /자동 검색|연결 완료/);
});

test("only a current successful empty observation reports no detected CLI", () => {
  const view = formatClaudeRemoteControlStatus(status({ registered: 0, waiting: 0 }), NOW);
  assert.deepEqual(view.details, ["감지된 Claude CLI 세션이 없습니다."]);
});

test("RC monitor errors and stale observations qualify retained counts and suppress old retry promises", () => {
  for (const patch of [{ error: "synthetic error" }, { checked_at: NOW - 31 }]) {
    const view = formatClaudeRemoteControlStatus(status({ ...patch, retry_at: NOW + 3 }), NOW);
    assert.match(view.summary, /실행 세션 1.*연결 확인 0/);
    assert.match(view.details[0], /마지막 (보고|관측)값/);
    assert.equal(view.retryNotice, null);
    assert.doesNotMatch(view.details.join(" "), /자동 검색/);
  }
  const fresh = formatClaudeRemoteControlStatus(status({ checked_at: NOW - 30, retry_at: NOW + 3 }), NOW);
  assert.match(fresh.retryNotice!, /3초 후/);
});

test("RC handles future backend reasons safely and ignores zero or invalid reason counts", () => {
  const reasons = JSON.parse('{"future_reason":2,"toString":1,"__proto__":1,"busy":0,"shell":-1,"permission_required":1.5}');
  const view = formatClaudeRemoteControlStatus(status({ waiting_reasons: reasons }), NOW);
  assert.equal(view.details.length, 3);
  for (const detail of view.details) assert.match(detail, /대기 사유의 세부 내용을 아직 확인하지 못/);
  assert.doesNotMatch(view.details.join(" "), /future_reason|toString|__proto__|function/);
});

test("an automatic recheck countdown is independent of sent requests and confirmed connections", () => {
  const original = status({ waiting_reasons: { waiting_for_stable_prompt: 1 }, retry_at: NOW + 5 });
  for (const [offset, expected] of [[0, 5], [1, 4], [4.5, 1]] as const) {
    const view = formatClaudeRemoteControlStatus(original, NOW + offset);
    assert.equal(view.retryNotice, `자동 재확인 ${expected}초 후 (횟수 제한)`);
    assert.match(view.summary, /연결 확인 0 · 요청 후 확인 대기 0/);
    assert.doesNotMatch(view.retryNotice!, /자동 연결|재연결 요청|연결 완료/);
  }
  for (const now of [NOW + 5, NOW + 6]) {
    const view = formatClaudeRemoteControlStatus(original, now);
    assert.equal(view.retryNotice, "자동 재확인 예정 시각이 지났습니다. 결과 갱신을 기다립니다.");
    assert.doesNotMatch(view.retryNotice!, /재확인 중|연결 확인됨|연결 완료/);
    assert.match(view.summary, /연결 확인 0 · 요청 후 확인 대기 0/);
  }
});

test("RC never invents a recheck when the bounded retry budget has no scheduled deadline", () => {
  for (const retry_at of [undefined, null, 0, -1, NaN, Infinity]) {
    const view = formatClaudeRemoteControlStatus(status({ requested: 1, retry_at }), NOW);
    assert.match(view.summary, /요청 후 확인 대기 1/);
    assert.equal(view.retryNotice, null);
  }
});
