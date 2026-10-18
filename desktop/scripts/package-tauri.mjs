import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktopDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const workspaceDir = path.resolve(desktopDir, "..");
const pkg = JSON.parse(fs.readFileSync(path.join(desktopDir, "package.json"), "utf8"));
const mode = process.argv[2] || "dir";

// 앱 버전이나 저장소 위치가 바뀌어도 기존 폴더 배포본의 위치를 유지한다.
const releaseDir = "C:\\Projects\\lof-library-daemon\\release";
const unpackedDir = path.join(releaseDir, "win-unpacked");
const releaseTarget = path.join(workspaceDir, "target", "release");
const builtExe = path.join(releaseTarget, process.platform === "win32" ? "lof-library-desktop.exe" : "lof-library-desktop");
const targetExeName = process.platform === "win32" ? "Lof Library.exe" : "Lof Library";

function assertChildPath(parent, target) {
  const base = fs.realpathSync(parent);
  const resolved = fs.realpathSync(target);
  const relative = path.relative(base, resolved);
  if (!relative || relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`배포 폴더 밖의 경로는 지우지 않습니다: ${resolved}`);
  }
}

if (!["dir", "nsis"].includes(mode)) {
  console.error(`지원하지 않는 배포 방식입니다: ${mode} (dir 또는 nsis)`);
  process.exit(1);
}

const tauriCli = path.join(desktopDir, "node_modules", "@tauri-apps", "cli", "tauri.js");
if (!fs.existsSync(tauriCli)) {
  console.error("Tauri CLI가 없습니다. desktop 폴더에서 npm install을 먼저 실행해 주세요.");
  process.exit(1);
}

const tauriArgs = mode === "nsis" ? [tauriCli, "build", "--bundles", "nsis"] : [tauriCli, "build", "--no-bundle"];
const result = spawnSync(process.execPath, tauriArgs, { cwd: desktopDir, stdio: "inherit" });
if (result.status !== 0) {
  process.exit(result.status ?? 1);
}
if (!fs.existsSync(builtExe)) {
  console.error(`빌드한 실행 파일을 찾지 못했습니다: ${builtExe}`);
  process.exit(1);
}

// 실행 중인 이전 배포본이 있으면 파일을 교체할 수 없으므로 먼저 종료한다.
if (process.platform === "win32") {
  spawnSync("taskkill", ["/F", "/IM", targetExeName], { stdio: "ignore" });
}

fs.mkdirSync(unpackedDir, { recursive: true });
assertChildPath(releaseDir, unpackedDir);
for (const entry of fs.readdirSync(unpackedDir)) {
  assertChildPath(unpackedDir, path.join(unpackedDir, entry));
}
for (const entry of fs.readdirSync(unpackedDir)) {
  fs.rmSync(path.join(unpackedDir, entry), { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
fs.copyFileSync(builtExe, path.join(unpackedDir, targetExeName));

if (mode === "nsis") {
  const nsisDir = path.join(releaseTarget, "bundle", "nsis");
  for (const file of fs.existsSync(nsisDir) ? fs.readdirSync(nsisDir) : []) {
    if (file.endsWith(".exe")) {
      fs.copyFileSync(path.join(nsisDir, file), path.join(releaseDir, file));
      console.log(`설치 파일: ${path.join(releaseDir, file)}`);
    }
  }
}

console.log(`Lof Library ${pkg.version} 폴더 배포본: ${unpackedDir}`);
