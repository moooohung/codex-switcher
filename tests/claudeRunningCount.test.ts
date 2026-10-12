import assert from "node:assert/strict";
import test from "node:test";
import { formatClaudeRunningStatus, type ClaudeSnapshot } from "../src/lib/claude.ts";

function snapshot(patch: Partial<ClaudeSnapshot> = {}): ClaudeSnapshot {
  return { managed_running_ids: [], running_process_count: 1, running_process_error: null,
    remote_control_status: { checked_at: 100, registered: 1, connected: 0, requested: 0,
      waiting: 1, runtime_bound: 1, transport: "synthetic", error: null },
    ...patch } as ClaudeSnapshot;
}

test("external live Claude counts while RC remains busy and unconnected", () => {
  const state = snapshot();
  assert.deepEqual(formatClaudeRunningStatus(state), { count: 1, complete: true, label: "1 Claude running" });
  assert.equal(state.managed_running_ids.length, 0);
  assert.equal(state.remote_control_status?.connected, 0);
  assert.equal(state.remote_control_status?.waiting, 1);
});

test("managed IDs and RC bindings cannot increase a backend-confirmed process count", () => {
  const state = snapshot({ managed_running_ids: ["managed-a", "managed-a"],
    remote_control_status: { ...snapshot().remote_control_status!, registered: 7, runtime_bound: 6 } });
  const before = structuredClone(state);
  assert.equal(formatClaudeRunningStatus(state)?.count, 1);
  assert.deepEqual(state, before);
});

test("stale RC registration cannot resurrect an exited native Claude", () => {
  assert.deepEqual(formatClaudeRunningStatus(snapshot({ running_process_count: 0 })),
    { count: 0, complete: true, label: "0 Claude running" });
});

test("an older snapshot without a total never presents managed-only zero as a full running count", () => {
  for (const managed_running_ids of [[], ["managed-a"]]) {
    const view = formatClaudeRunningStatus(snapshot({ managed_running_ids, running_process_count: undefined }));
    assert.equal(view?.count, null);
    assert.equal(view?.complete, false);
    assert.match(view!.label, /전체 확인 대기/);
    assert.doesNotMatch(view!.label, /0 Claude running/);
  }
  assert.equal(formatClaudeRunningStatus(null), null);
});

test("failed discovery preserves a confirmed lower bound and never claims confirmed absence", () => {
  const error = "synthetic discovery error";
  const partial = formatClaudeRunningStatus(snapshot({ running_process_error: error }));
  assert.deepEqual(partial, { count: 1, complete: false, label: "1 Claude running · 일부 확인 실패" });
  const unknown = formatClaudeRunningStatus(snapshot({ running_process_count: 0, running_process_error: error }));
  assert.equal(unknown?.complete, false);
  assert.equal(unknown?.label, "Claude running: 확인 실패");
});

test("failed refresh qualifies retained observations rather than asserting a current count", () => {
  for (const running_process_count of [0, 1]) {
    const view = formatClaudeRunningStatus(snapshot({ running_process_count }), true);
    assert.equal(view?.complete, false);
    assert.equal(view?.label, `${running_process_count} Claude running · 마지막 관측값`);
  }
});

test("invalid backend totals are unavailable rather than coerced into a truthful count", () => {
  for (const running_process_count of [-1, 0.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1]) {
    assert.deepEqual(formatClaudeRunningStatus(snapshot({ running_process_count })),
      { count: null, complete: false, label: "Claude running: 전체 확인 대기" });
  }
});
