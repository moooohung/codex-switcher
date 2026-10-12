import { useEffect, useState, type ReactNode } from "react";
import { isTauriRuntime, WEB_TOKEN_KEY } from "../lib/platform";

export function WebAccessGate({ children }: { children: ReactNode }) {
  const [authorized, setAuthorized] = useState(isTauriRuntime());
  const [token, setToken] = useState(() => sessionStorage.getItem(WEB_TOKEN_KEY) ?? "");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const authenticate = async (value: string) => {
    setBusy(true); setError("");
    try {
      const response = await fetch("/api/health", { headers: { Authorization: `Bearer ${value}` } });
      if (!response.ok) throw new Error("접근 토큰을 확인하세요.");
      sessionStorage.setItem(WEB_TOKEN_KEY, value); setAuthorized(true);
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(false); }
  };
  useEffect(() => {
    if (!isTauriRuntime() && token) void authenticate(token);
    const expired = () => { sessionStorage.removeItem(WEB_TOKEN_KEY); setAuthorized(false); setToken(""); };
    window.addEventListener("web-access-expired", expired);
    return () => window.removeEventListener("web-access-expired", expired);
  }, []);
  if (authorized) return <>{children}</>;
  return <main className="min-h-screen flex items-center justify-center p-6"><form className="max-w-lg w-full space-y-4" onSubmit={(e) => { e.preventDefault(); void authenticate(token.trim()); }}>
    <h1 className="text-xl font-bold">Codex Switcher 웹 접근</h1>
    <p>서버의 .codex-switcher/web-access-token 파일에 저장된 접근 토큰을 입력하세요.</p>
    <label className="block">접근 토큰<input className="block w-full border rounded p-2 dark:bg-gray-800" type="password" autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)} /></label>
    {error && <p role="alert">{error}</p>}<button disabled={busy || !token.trim()} className="bg-blue-600 text-white rounded px-4 py-2">{busy ? "확인 중…" : "접속"}</button>
  </form></main>;
}
