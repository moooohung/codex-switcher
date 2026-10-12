import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";

export const REPOSITORY = "moooohung/codex-switcher";
export const TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"];
const SHA = /^[a-f0-9]{40}$/;
const VERSION = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const PACKAGE = /(?:\.dmg|\.msi|\.exe|\.deb|\.rpm|\.AppImage|\.zip|\.tar\.gz)$/i;

function readRegularFile(file) {
  // Validate and read one open descriptor, rather than checking a pathname
  // and opening it again. Publication runs on Linux, where O_NOFOLLOW also
  // rejects symlinks. The descriptor check remains portable to Windows.
  const fd = fs.openSync(file, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW ?? 0));
  try {
    assert(fs.fstatSync(fd).isFile(), "Release assets must be regular files");
    return fs.readFileSync(fd);
  } finally { fs.closeSync(fd); }
}

export function releasePlan({ repository, eventName, event, sha, version, requestedTag = "", workflowSha = sha }) {
  assert.equal(repository, REPOSITORY, "Release repository is not trusted");
  assert.match(version, VERSION, "Source must have a stable package version");
  assert.match(sha, SHA, "Release requires a full source SHA");
  if (eventName === "workflow_run") {
    const run = event.workflow_run;
    if (run?.conclusion !== "success" || run.event !== "push" || run.head_branch !== "main" ||
        run.head_repository?.full_name !== repository || run.name !== "CI") return null;
    assert.equal(run.head_sha, sha, "Release source differs from validated source");
    // workflow_run signs the default-branch digest, not its triggering run.
    // Skip superseded commits so attestations bind to the actual built source.
    if (workflowSha !== sha) return null;
  } else if (eventName === "workflow_dispatch") {
    assert(["main", "refs/heads/main"].includes(event.ref), "Manual releases require main");
  } else if (eventName === "push") {
    assert.equal(event.repository?.full_name, repository);
    assert.equal(event.ref, `refs/tags/v${version}`, "Tag and package version must agree");
    requestedTag = `v${version}`;
  } else {
    return null;
  }
  if (requestedTag) assert.equal(requestedTag, `v${version}`, "Stable release tag must match the source version");
  const buildVersion = requestedTag ? version : `${version}-ci.g${sha.slice(0, 12)}`;
  return { sha, version: buildVersion, tag: `v${buildVersion}`, prerelease: !requestedTag };
}

export function wixVersion(version, runNumber) {
  const match = version.match(/^(\d+)\.(\d+)\.(\d+)(-ci\.g[a-f0-9]{12})?$/);
  assert(match, "Invalid installer source version");
  const parts = match.slice(1, 4).map(Number);
  assert(parts[0] <= 255 && parts[1] <= 255 && parts[2] <= 65535, "MSI version component exceeds WiX limits");
  // Stable builds sort above previews of the same application version.
  // Reject counter overflow rather than silently reuse an installer version.
  const build = match[4] ? Number(runNumber) : 65535;
  assert(Number.isInteger(build) && build >= 1 && build <= (match[4] ? 65534 : 65535), "MSI preview requires a run number between 1 and 65534");
  return [...parts, build].join(".");
}

export function versionEdits(root, version, runNumber) {
  const files = ["package.json", "src-tauri/tauri.conf.json", "src-tauri/Cargo.toml", "src-tauri/Cargo.lock"];
  const texts = files.map((file) => fs.readFileSync(path.join(root, file), "utf8"));
  const old = JSON.parse(texts[0]).version;
  assert.match(old, VERSION);
  assert.equal(JSON.parse(texts[1]).version, old, "Tauri version drift");
  const cargo = /(^\[package\][\s\S]*?^version\s*=\s*")([^"\r\n]+)(")/m;
  const lock = /(\[\[package\]\]\r?\nname = "codex-switcher"\r?\nversion = ")([^"\r\n]+)(")/;
  assert.equal(texts[2].match(cargo)?.[2], old, "Cargo version drift");
  assert.equal(texts[3].match(lock)?.[2], old, "Cargo lock version drift");
  assert.match(version, /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-ci\.g[a-f0-9]{12})?$/);
  const edits = new Map([
    [files[0], texts[0].replace(/"version"\s*:\s*"[^"\r\n]+"/, `"version": "${version}"`)],
    [files[1], texts[1].replace(/"version"\s*:\s*"[^"\r\n]+"/, `"version": "${version}"`)],
    [files[2], texts[2].replace(cargo, (_, before, _old, after) => before + version + after)],
    [files[3], texts[3].replace(lock, (_, before, _old, after) => before + version + after)],
  ]);
  if (runNumber !== undefined) {
    const config = JSON.parse(edits.get(files[1]));
    config.bundle ??= {};
    config.bundle.windows ??= {};
    config.bundle.windows.wix ??= {};
    config.bundle.windows.wix.version = wixVersion(version, runNumber);
    edits.set(files[1], JSON.stringify(config, null, 2) + "\n");
  }
  return edits;
}

export function collectReleaseFiles(root, plan) {
  const files = fs.readdirSync(root).filter((name) => name !== "checksums.txt").sort();
  const found = new Set();
  const declared = new Set();
  for (const name of files) {
    const file = path.join(root, name);
    const bytes = readRegularFile(file);
    assert(!/[\r\n\0]/.test(name));
    if (name.startsWith("provenance-") && name.endsWith(".json")) {
      const manifest = JSON.parse(bytes.toString("utf8"));
      assert(TARGETS.includes(manifest.target));
      assert.equal(name, `provenance-${manifest.target}.json`);
      assert.equal(manifest.sha, plan.sha);
      assert.equal(manifest.version, plan.version);
      assert.equal(manifest.tag, plan.tag);
      assert(!found.has(manifest.target), "Duplicate target provenance");
      assert(Array.isArray(manifest.files) && manifest.files.length > 0);
      for (const asset of manifest.files) {
        assert.equal(path.basename(asset), asset);
        assert(asset.startsWith(`${manifest.target}--`) && PACKAGE.test(asset));
        assert(files.includes(asset), "Missing platform package");
        assert(!declared.has(asset), "Duplicate package declaration");
        declared.add(asset);
      }
      found.add(manifest.target);
    } else assert(PACKAGE.test(name), "Unexpected release file");
  }
  assert.equal(found.size, TARGETS.length, "All four platforms must succeed before publishing");
  for (const name of files.filter((name) => PACKAGE.test(name))) {
    assert(declared.has(name), "Package is not declared by platform provenance");
  }
  return files.map((name) => path.join(root, name));
}

const run = (command, args) => execFileSync(command, args, { encoding: "utf8", maxBuffer: 16 * 1024 * 1024, stdio: ["ignore", "pipe", "pipe"] }).trim();
const api = (route, execute = run) => JSON.parse(execute("gh", ["api", route]));
function optionalApi(route, execute = run) {
  try { return api(route, execute); } catch (error) {
    if (String(error.stderr).includes("HTTP 404")) return null;
    throw error;
  }
}
function checkTag(plan, execute = run) {
  let object = optionalApi(`repos/${REPOSITORY}/git/ref/tags/${plan.tag}`, execute)?.object;
  for (let count = 0; object?.type === "tag" && count < 4; count++) object = api(`repos/${REPOSITORY}/git/tags/${object.sha}`, execute).object;
  if (object) assert(object.type === "commit" && object.sha === plan.sha, "Existing tag belongs to another source commit");
}
function requireCI(sha, execute = run) {
  const runs = api(`repos/${REPOSITORY}/actions/workflows/pull-request-validation.yml/runs?head_sha=${sha}&status=success&per_page=100`, execute).workflow_runs;
  assert(runs.some((r) => r.head_sha === sha && r.head_branch === "main" &&
    r.head_repository?.full_name === REPOSITORY && ["push", "workflow_dispatch"].includes(r.event)), "This exact source has no successful trusted main CI run");
}
function output(values) {
  fs.appendFileSync(process.env.GITHUB_OUTPUT, Object.entries(values).map(([k, v]) => `${k}=${v}\n`).join(""));
}

export function findRelease(tag, execute = run) {
  // The tag endpoint may return 404 for a draft. Authenticated release listings
  // include writable drafts; paginate so retrying an older draft finds its ID.
  const pages = JSON.parse(execute("gh", ["api", `repos/${REPOSITORY}/releases?per_page=100`, "--paginate", "--slurp"]));
  const matches = pages.flat().filter((release) => release.tag_name === tag);
  assert(matches.length <= 1, "Multiple releases have the same tag");
  return matches[0] ?? null;
}

export function publishRelease({ repository, sha, tag, version, prerelease, root = process.cwd() }, execute = run) {
  const plan = { sha, tag, version };
  assert.equal(repository, REPOSITORY);
  assert.match(sha, SHA);
  assert.match(tag, /^v\d+\.\d+\.\d+(-ci\.g[a-f0-9]{12})?$/);
  assert.equal(tag, `v${version}`, "Release tag and build version must agree");
  assert.equal(typeof prerelease, "boolean");
  requireCI(sha, execute);
  checkTag(plan, execute);
  const files = collectReleaseFiles(path.join(root, "release-assets"), plan);
  // Verify provenance before publication, including the exact source digest.
  for (const file of files) execute("gh", ["attestation", "verify", file, "--repo", REPOSITORY, "--source-digest", sha, "--signer-workflow", `${REPOSITORY}/.github/workflows/build.yml`, "--deny-self-hosted-runners"]);
  const digests = new Map(files.map((file) => [path.basename(file), createHash("sha256").update(readRegularFile(file)).digest("hex")]));
  const checksumFile = path.join(root, "release-assets/checksums.txt");
  fs.writeFileSync(checksumFile, [...digests].map(([name, digest]) => `${digest}  ${name}\n`).join(""));
  digests.set("checksums.txt", createHash("sha256").update(fs.readFileSync(checksumFile)).digest("hex"));
  let release = findRelease(tag, execute);
  if (release && !release.draft) return;
  if (!release) {
    const notesFile = path.join(root, "release-notes.md");
    fs.writeFileSync(notesFile, `Source: ${sha}\n\nAll four platform bundles passed. SHA-256 checksums and GitHub build attestations are included. OS certificate signing/notarization is not configured.\n`);
    // Use the create response: authenticated listings can lag behind creation.
    release = JSON.parse(execute("gh", ["api", `repos/${REPOSITORY}/releases`, "--method", "POST",
      "-f", `tag_name=${tag}`, "-f", `target_commitish=${sha}`, "-f", `name=${tag}`,
      "-F", "draft=true", "-F", `prerelease=${prerelease}`, "-f", `body=${fs.readFileSync(notesFile, "utf8")}`]));
  }
  assert.equal(release?.draft, true, "Created release is not an authenticated draft");
  assert(Number.isSafeInteger(release.id) && release.id > 0, "Invalid draft release ID");
  assert.equal(release.tag_name, tag, "Draft release belongs to another tag");
  assert.equal(release.target_commitish, sha, "Draft release belongs to another source commit");
  const releaseId = release.id;
  const draft = api(`repos/${REPOSITORY}/releases/${releaseId}`, execute);
  assert.equal(draft.id, releaseId, "Draft release ID changed before upload");
  assert.equal(draft.draft, true, "Release was published before upload");
  assert.equal(draft.tag_name, tag, "Draft release tag changed before upload");
  assert.equal(draft.target_commitish, sha, "Draft release source changed before upload");
  checkTag(plan, execute);
  execute("gh", ["release", "upload", tag, ...files, checksumFile, "--repo", REPOSITORY, "--clobber"]);
  release = api(`repos/${REPOSITORY}/releases/${releaseId}`, execute);
  assert.equal(release.id, releaseId, "Release ID changed during upload");
  assert.equal(release.tag_name, tag, "Release tag changed during upload");
  assert.equal(release.target_commitish, sha, "Release source changed during upload");
  assert.equal(release.draft, true, "Release was published before asset verification");
  assert.equal(release.assets.length, digests.size, "Unexpected or missing published assets");
  assert.equal(new Set(release.assets.map((asset) => asset.name)).size, digests.size, "Duplicate uploaded asset names");
  for (const asset of release.assets) assert.equal(asset.digest, `sha256:${digests.get(asset.name)}`, "Uploaded asset digest mismatch");
  checkTag(plan, execute);
  execute("gh", ["release", "edit", tag, "--repo", REPOSITORY, "--draft=false", `--prerelease=${prerelease}`, `--latest=${!prerelease}`]);
  return releaseId;
}

function main(command) {
  const root = process.cwd();
  if (command === "plan") {
    const event = JSON.parse(fs.readFileSync(process.env.GITHUB_EVENT_PATH, "utf8"));
    const sha = process.env.SOURCE_SHA;
    const version = JSON.parse(fs.readFileSync("package.json", "utf8")).version;
    const plan = releasePlan({ repository: process.env.GITHUB_REPOSITORY, eventName: process.env.GITHUB_EVENT_NAME, event, sha, version, requestedTag: process.env.REQUESTED_TAG, workflowSha: process.env.GITHUB_SHA });
    if (!plan) return output({ ready: false });
    versionEdits(root, plan.version, process.env.GITHUB_RUN_NUMBER); // Check all files before any build mutation.
    requireCI(sha);
    checkTag(plan);
    const existing = findRelease(plan.tag);
    if (existing && !existing.draft) return output({ ready: false });
    output({ ready: true, ...plan });
  } else if (command === "apply") {
    const edits = versionEdits(root, process.env.BUILD_VERSION, process.env.GITHUB_RUN_NUMBER);
    for (const [file, text] of edits) fs.writeFileSync(path.join(root, file), text);
  } else if (command === "stage") {
    const target = process.env.BUILD_TARGET;
    assert(TARGETS.includes(target));
    const paths = JSON.parse(process.env.ARTIFACT_PATHS);
    assert(Array.isArray(paths));
    fs.mkdirSync("release-assets", { recursive: true });
    const files = [];
    for (const source of paths) {
      if (!PACKAGE.test(source)) continue;
      const bytes = readRegularFile(source);
      const name = `${target}--${path.basename(source)}`;
      assert(!/[\r\n\0]/.test(name));
      const destination = path.join("release-assets", name);
      fs.writeFileSync(destination, bytes, { flag: "wx" });
      files.push(name);
    }
    assert(files.length > 0, "Tauri produced no distributable packages");
    fs.writeFileSync(`release-assets/provenance-${target}.json`, JSON.stringify({ target, sha: process.env.SOURCE_SHA, tag: process.env.RELEASE_TAG, version: process.env.BUILD_VERSION, files }, null, 2) + "\n");
  } else if (command === "publish") {
    const releaseId = publishRelease({ repository: process.env.GITHUB_REPOSITORY, sha: process.env.SOURCE_SHA, tag: process.env.RELEASE_TAG, version: process.env.BUILD_VERSION, prerelease: process.env.PRERELEASE === "true" });
    if (releaseId) console.log(`Published ${process.env.RELEASE_TAG} from ${process.env.SOURCE_SHA}`);
  } else throw new Error("Expected plan, apply, stage or publish");
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) main(process.argv[2]);
