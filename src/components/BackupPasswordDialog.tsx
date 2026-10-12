import { useEffect, useState } from "react";

type Mode = "export" | "import";
type Request = { mode: Mode; resolve: (password: string | null) => void };
let pending: Request | null = null;
export function requestBackupPassword(mode: Mode): Promise<string | null> {
  pending?.resolve(null);
  return new Promise((resolve) => {
    pending = { mode, resolve };
    window.dispatchEvent(new Event("backup-password-request"));
  });
}
export function BackupPasswordDialog() {
  const [request, setRequest] = useState<Request | null>(null);
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  useEffect(() => {
    const listener = () => { setRequest(pending); setPassword(""); setConfirmation(""); };
    window.addEventListener("backup-password-request", listener);
    return () => { window.removeEventListener("backup-password-request", listener); pending?.resolve(null); pending = null; };
  }, []);
  if (!request) return null;
  const finish = (value: string | null) => { request.resolve(value); pending = null; setRequest(null); setPassword(""); setConfirmation(""); };
  const valid = request.mode === "import" || (Array.from(password).length >= 12 && password === confirmation);
  return <div className="fixed inset-0 z-[100] bg-black/50 flex items-center justify-center" role="dialog" aria-modal="true" aria-labelledby="backup-password-title">
    <form className="bg-white dark:bg-gray-900 rounded-xl p-6 m-4 w-full max-w-md space-y-4" onSubmit={(e) => { e.preventDefault(); if (valid) finish(password); }}>
      <h2 id="backup-password-title" className="text-lg font-semibold">백업 암호 {request.mode === "export" ? "설정" : "입력"}</h2>
      <p className="text-sm">{request.mode === "export" ? "12자 이상 입력하세요. 복원할 때 이 암호가 필요합니다." : "내보낼 때 설정한 암호를 입력하세요. 구버전 백업은 비워 두세요."}</p>
      <label className="block">암호<input autoFocus type="password" autoComplete="new-password" value={password} onChange={(e) => setPassword(e.target.value)} className="block w-full border rounded p-2 dark:bg-gray-800" /></label>
      {request.mode === "export" && <label className="block">암호 확인<input type="password" autoComplete="new-password" value={confirmation} onChange={(e) => setConfirmation(e.target.value)} className="block w-full border rounded p-2 dark:bg-gray-800" /></label>}
      <div className="flex gap-3 justify-end"><button type="button" onClick={() => finish(null)}>취소</button><button type="submit" disabled={!valid} className="px-4 py-2 bg-blue-600 text-white rounded disabled:opacity-40">확인</button></div>
    </form>
  </div>;
}
