import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { REPOSITORY, TARGETS, releasePlan, versionEdits, collectReleaseFiles } from "../scripts/ci-release.mjs";

const sha = "abcdef1234567890abcdef1234567890abcdef12";
const trusted = { name: "CI", event: "push", conclusion: "success", head_branch: "main", head_sha: sha, head_repository: { full_name: REPOSITORY } };
const input = { repository: REPOSITORY, eventName: "workflow_run", event: { workflow_run: trusted }, sha, version: "0.2.21" };

test("only a successful trusted main push can automatically release its exact SHA", () => {
  const plan = releasePlan(input);
  assert.deepEqual(plan, { sha, version: "0.2.21-ci.gabcdef123456", tag: "v0.2.21-ci.gabcdef123456", prerelease: true });
  for (const patch of [{ event: "pull_request" }, { event: "workflow_dispatch" }, { conclusion: "failure" }, { conclusion: "cancelled" }, { head_branch: "feature" }, { name: "Other CI" }, { head_repository: { full_name: "other/fork" } }]) {
    assert.equal(releasePlan({ ...input, event: { workflow_run: { ...trusted, ...patch } } }), null);
  }
  assert.throws(() => releasePlan({ ...input, sha: "0".repeat(40) }), /validated source/);
  assert.throws(() => releasePlan({ ...input, repository: "other/fork" }), /trusted/);
  assert.equal(releasePlan({ ...input, workflowSha: "0".repeat(40) }), null);
});

test("tags must exactly match stable source versions; preview identifiers are deterministic", () => {
  assert.equal(releasePlan(input).tag, releasePlan(input).tag);
  const event = { ref: "refs/tags/v0.2.21", repository: { full_name: REPOSITORY } };
  assert.equal(releasePlan({ ...input, eventName: "push", event }).prerelease, false);
  assert.throws(() => releasePlan({ ...input, eventName: "push", event: { ...event, ref: "refs/tags/v9.9.9" } }));
  assert.throws(() => releasePlan({ ...input, eventName: "workflow_dispatch", event: { ref: "feature" } }));
  assert.equal(releasePlan({ ...input, eventName: "workflow_dispatch", event: { ref: "refs/heads/main" } }).sha, sha);
  assert.throws(() => releasePlan({ ...input, eventName: "workflow_dispatch", event: {} }));
  assert.throws(() => releasePlan({ ...input, eventName: "workflow_dispatch", event: { ref: "main" }, requestedTag: "v0.2.21; echo secret" }));
});

test("all version documents are validated before any file is mutated", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "switcher-ci-version-"));
  try {
    fs.mkdirSync(path.join(root, "src-tauri"));
    const originals = new Map([
      ["package.json", '{"name":"codex-switcher","version":"0.2.21"}\n'],
      ["src-tauri/tauri.conf.json", '{"version":"0.2.21"}\n'],
      ["src-tauri/Cargo.toml", '[package]\nname = "codex-switcher"\nversion = "0.2.21"\n'],
      ["src-tauri/Cargo.lock", '[[package]]\nname = "codex-switcher"\nversion = "0.2.21"\n'],
    ]);
    for (const [file, text] of originals) fs.writeFileSync(path.join(root, file), text);
    const edits = versionEdits(root, releasePlan(input).version);
    assert.equal(edits.size, 4);
    for (const [file, text] of originals) assert.equal(fs.readFileSync(path.join(root, file), "utf8"), text);
    fs.writeFileSync(path.join(root, "src-tauri/Cargo.lock"), '[[package]]\nname = "unrelated"\nversion = "0.2.21"\n');
    assert.throws(() => versionEdits(root, "0.2.21"), /lock version drift/);
    assert.equal(fs.readFileSync(path.join(root, "package.json"), "utf8"), originals.get("package.json"));
  } finally { fs.rmSync(root, { recursive: true }); }
});

test("publication requires complete platform manifests matching the same source and version", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "switcher-ci-assets-"));
  const plan = releasePlan(input);
  try {
    for (const target of TARGETS) {
      const file = `${target}--fixture.zip`;
      fs.writeFileSync(path.join(root, file), "synthetic package");
      fs.writeFileSync(path.join(root, `provenance-${target}.json`), JSON.stringify({ target, ...plan, files: [file] }));
    }
    assert.equal(collectReleaseFiles(root, plan).length, 8);
    const extra = path.join(root, `${TARGETS[0]}--unlisted.zip`);
    fs.writeFileSync(extra, "unlisted package");
    assert.throws(() => collectReleaseFiles(root, plan), /not declared/);
    fs.unlinkSync(extra);
    fs.mkdirSync(extra);
    assert.throws(() => collectReleaseFiles(root, plan), /regular files/);
    fs.rmdirSync(extra);
    if (process.platform !== "win32") {
      fs.symlinkSync(path.join(root, `${TARGETS[0]}--fixture.zip`), extra);
      assert.throws(() => collectReleaseFiles(root, plan), /ELOOP/);
      fs.unlinkSync(extra);
    }
    const missingManifest = path.join(root, `provenance-${TARGETS[3]}.json`);
    const savedManifest = fs.readFileSync(missingManifest);
    fs.unlinkSync(missingManifest);
    assert.throws(() => collectReleaseFiles(root, plan), /four platforms/);
    fs.writeFileSync(missingManifest, savedManifest);
    const target = TARGETS[0];
    fs.unlinkSync(path.join(root, `${target}--fixture.zip`));
    assert.throws(() => collectReleaseFiles(root, plan), /Missing platform package/);
    fs.writeFileSync(path.join(root, `${target}--fixture.zip`), "synthetic package");
    const file = path.join(root, `provenance-${target}.json`);
    fs.writeFileSync(file, JSON.stringify({ target, ...plan, sha: "0".repeat(40), files: [`${target}--fixture.zip`] }));
    assert.throws(() => collectReleaseFiles(root, plan));
  } finally { fs.rmSync(root, { recursive: true }); }
});
