import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { REPOSITORY, TARGETS, releasePlan, wixVersion, versionEdits, collectReleaseFiles, findRelease, publishRelease } from "../scripts/ci-release.mjs";

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
    const installer = versionEdits(root, releasePlan(input).version, 42);
    const config = JSON.parse(installer.get("src-tauri/tauri.conf.json"));
    assert.equal(config.version, releasePlan(input).version);
    assert.equal(config.bundle.windows.wix.version, "0.2.21.42");
    fs.writeFileSync(path.join(root, "src-tauri/Cargo.lock"), '[[package]]\nname = "unrelated"\nversion = "0.2.21"\n');
    assert.throws(() => versionEdits(root, "0.2.21"), /lock version drift/);
    assert.equal(fs.readFileSync(path.join(root, "package.json"), "utf8"), originals.get("package.json"));
  } finally { fs.rmSync(root, { recursive: true }); }
});

test("MSI preview versions are numeric, bounded and sort below the matching stable build", () => {
  assert.equal(wixVersion(releasePlan(input).version, 1), "0.2.21.1");
  assert.equal(wixVersion(releasePlan(input).version, 65534), "0.2.21.65534");
  assert.equal(wixVersion("0.2.21", 42), "0.2.21.65535");
  for (const run of [undefined, 0, -1, 65535, "NaN", 1.5]) {
    assert.throws(() => wixVersion(releasePlan(input).version, run));
  }
  assert.throws(() => wixVersion("256.0.1", 1), /WiX limits/);
  assert.throws(() => wixVersion("0.2.65536", 1), /WiX limits/);
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

function publisherFixture(t, options = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "switcher-ci-publisher-"));
  t.after(() => fs.rmSync(root, { recursive: true }));
  const plan = releasePlan(input);
  const assets = path.join(root, "release-assets");
  fs.mkdirSync(assets);
  for (const target of TARGETS) {
    const file = `${target}--fixture.zip`;
    fs.writeFileSync(path.join(assets, file), `synthetic package for ${target}`);
    fs.writeFileSync(path.join(assets, `provenance-${target}.json`), JSON.stringify({ target, ...plan, files: [file] }));
  }
  let release = options.existing === false ? null : {
    id: 410176291, tag_name: plan.tag, target_commitish: options.draftSha ?? sha,
    draft: options.published !== true, assets: [],
  };
  const calls = [];
  const notFound = () => { throw Object.assign(new Error("Not Found"), { stderr: "gh: Not Found (HTTP 404)" }); };
  const fakeGh = (command, args) => {
    assert.equal(command, "gh");
    calls.push([...args]);
    if (args[0] === "api") {
      const route = args[1];
      // Reproduce the production failure: even the authenticated token cannot
      // read this draft through releases/tags/<tag>.
      if (route.includes("/releases/tags/")) return notFound();
      if (route.includes("/actions/workflows/")) return JSON.stringify({ workflow_runs: [{ ...trusted, head_sha: options.ciSha ?? sha }] });
      if (route.includes("/git/ref/tags/")) return JSON.stringify({ object: { type: "commit", sha: options.tagSha ?? sha } });
      if (route === `repos/${REPOSITORY}/releases?per_page=100`) {
        assert.deepEqual(args.slice(2), ["--paginate", "--slurp"]);
        if (options.listError) throw options.listError;
        return JSON.stringify([[{ id: 1, tag_name: "unrelated", draft: false }], release ? [release] : []]);
      }
      if (route === `repos/${REPOSITORY}/releases/410176291`) {
        assert(calls.some((call) => call[0] === "release" && call[1] === "upload"));
        return JSON.stringify(options.readback ? options.readback(structuredClone(release)) : release);
      }
    } else if (args[0] === "attestation") {
      assert.equal(args[args.indexOf("--source-digest") + 1], sha);
      assert(args.includes("--deny-self-hosted-runners"));
      if (options.attestationError) throw new Error("Attestation verification failed");
      return "verified";
    } else if (args[0] === "release") {
      assert.equal(args[2], plan.tag);
      if (args[1] === "create") {
        assert.equal(release, null, "An existing draft must be reused");
        assert.equal(args[args.indexOf("--target") + 1], sha);
        assert(args.includes("--draft"));
        assert(args.includes("--prerelease"));
        release = { id: 410176291, tag_name: plan.tag, target_commitish: sha, draft: true, assets: [] };
        return "created";
      }
      if (args[1] === "upload") {
        assert.equal(release.draft, true);
        release.assets = args.slice(3, args.indexOf("--repo")).map((file) => ({
          name: path.basename(file), digest: `sha256:${createHash("sha256").update(fs.readFileSync(file)).digest("hex")}`,
        }));
        return "uploaded";
      }
      if (args[1] === "edit") {
        assert(calls.some((call) => call[0] === "api" && call[1].endsWith("/releases/410176291")), "Read back draft assets by ID before publishing");
        assert(args.includes("--draft=false") && args.includes("--prerelease=true") && args.includes("--latest=false"));
        release.draft = false;
        return "published";
      }
    }
    assert.fail(`Unexpected fake gh invocation: ${args.join(" ")}`);
  };
  return { args: { repository: REPOSITORY, ...plan, root }, fakeGh, calls, getRelease: () => release };
}

for (const existing of [false, true]) {
  test(`publisher ${existing ? "resumes" : "creates"} a draft hidden from the tag endpoint and verifies assets by ID`, (t) => {
    const fixture = publisherFixture(t, { existing });
    assert.throws(() => fixture.fakeGh("gh", ["api", `repos/${REPOSITORY}/releases/tags/${fixture.args.tag}`]), /Not Found/);
    fixture.calls.length = 0;
    assert.equal(publishRelease(fixture.args, fixture.fakeGh), 410176291);
    assert.equal(fixture.getRelease().draft, false);
    assert.equal(fixture.getRelease().assets.length, 9); // Four bundles, four manifests and checksums.
    assert.equal(fixture.calls.filter((call) => call[0] === "release" && call[1] === "create").length, existing ? 0 : 1);
    assert.equal(fixture.calls.filter((call) => call[0] === "attestation").length, 8);
    assert(!fixture.calls.some((call) => call[1]?.includes("/releases/tags/")));
    const upload = fixture.calls.findIndex((call) => call[0] === "release" && call[1] === "upload");
    assert(fixture.calls.slice(0, upload).filter((call) => call[0] === "attestation").length === 8);
  });
}

test("published releases are immutable and skipped without upload or publication", (t) => {
  const fixture = publisherFixture(t, { published: true });
  assert.equal(publishRelease(fixture.args, fixture.fakeGh), undefined);
  assert(!fixture.calls.some((call) => call[0] === "release"));
});

test("authenticated paginated release lookup handles absent and duplicate tags", () => {
  assert.equal(findRelease("missing", () => JSON.stringify([[{ tag_name: "other" }], []])), null);
  assert.throws(() => findRelease("duplicate", () => JSON.stringify([[{ tag_name: "duplicate" }], [{ tag_name: "duplicate" }]])), /Multiple releases/);
});

test("publisher keeps failed asset and identity readbacks as drafts", async (t) => {
  const cases = [
    ["missing asset", (r) => ({ ...r, assets: r.assets.slice(1) }), /missing published assets/],
    ["extra asset", (r) => ({ ...r, assets: [...r.assets, { name: "extra.zip", digest: "sha256:invalid" }] }), /Unexpected or missing/],
    ["wrong digest", (r) => ({ ...r, assets: r.assets.map((a, i) => i === 0 ? { ...a, digest: "sha256:invalid" } : a) }), /digest mismatch/],
    ["duplicate asset", (r) => ({ ...r, assets: r.assets.map((a, i) => i === 0 ? r.assets[1] : a) }), /Duplicate uploaded/],
    ["changed ID", (r) => ({ ...r, id: 2 }), /ID changed/],
    ["changed tag", (r) => ({ ...r, tag_name: "v9.9.9" }), /tag changed/],
    ["changed source", (r) => ({ ...r, target_commitish: "0".repeat(40) }), /source changed/],
  ];
  for (const [name, readback, error] of cases) await t.test(name, (t) => {
    const fixture = publisherFixture(t, { readback });
    assert.throws(() => publishRelease(fixture.args, fixture.fakeGh), error);
    assert.equal(fixture.getRelease().draft, true);
    assert(!fixture.calls.some((call) => call[0] === "release" && call[1] === "edit"));
  });
});

test("publisher retains trusted CI, source, tag and attestation gates", async (t) => {
  const cases = [
    ["unvalidated CI", { ciSha: "0".repeat(40) }, /successful trusted main CI/],
    ["conflicting tag", { tagSha: "0".repeat(40) }, /another source commit/],
    ["conflicting draft source", { draftSha: "0".repeat(40) }, /another source commit/],
    ["failed attestation", { attestationError: true }, /Attestation verification failed/],
    ["authentication failure", { listError: Object.assign(new Error("Unauthorized"), { stderr: "HTTP 401" }) }, /Unauthorized/],
  ];
  for (const [name, options, error] of cases) await t.test(name, (t) => {
    const fixture = publisherFixture(t, options);
    assert.throws(() => publishRelease(fixture.args, fixture.fakeGh), error);
    assert(!fixture.calls.some((call) => call[0] === "release"));
  });
});
