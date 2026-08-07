"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import axios from "axios";
import { Loader2, AlertCircle, Trash2, ShieldCheck, User as UserIcon, KeyRound, Copy, X } from "lucide-react";
import { api } from "../../lib/api";
import { useAuth, type User } from "../../lib/auth-context";

interface ResetKeyInfo {
  username: string;
  reset_key: string;
  expires_at: string;
}

export default function AdminUsersPage() {
  const { user, loading: authLoading } = useAuth();
  const router = useRouter();

  const [users, setUsers] = useState<User[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<number | null>(null);
  const [resetKeyInfo, setResetKeyInfo] = useState<ResetKeyInfo | null>(null);

  useEffect(() => {
    if (!authLoading && user && user.role !== "admin") {
      router.replace("/");
    }
  }, [authLoading, user, router]);

  useEffect(() => {
    if (!user || user.role !== "admin") return;
    let active = true;
    (async () => {
      try {
        const res = await api.get<User[]>("/users");
        if (active) setUsers(res.data);
      } catch (err) {
        console.error(err);
        if (active) setError("Failed to load users.");
      } finally {
        if (active) setLoading(false);
      }
    })();
    return () => {
      active = false;
    };
  }, [user]);

  const changeRole = async (target: User, role: "admin" | "user") => {
    setBusyId(target.id);
    setError(null);
    try {
      const res = await api.patch<User>(`/users/${target.id}`, { role });
      setUsers((prev) => prev.map((u) => (u.id === target.id ? res.data : u)));
    } catch (err) {
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to update role.");
    } finally {
      setBusyId(null);
    }
  };

  const issueResetKey = async (target: User) => {
    setBusyId(target.id);
    setError(null);
    try {
      const res = await api.post<ResetKeyInfo>(`/users/${target.id}/reset-password`);
      setResetKeyInfo(res.data);
    } catch (err) {
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to issue reset key.");
    } finally {
      setBusyId(null);
    }
  };

  const deleteUser = async (target: User) => {
    if (!confirm(`Delete user "${target.username}"? This cannot be undone.`)) return;
    setBusyId(target.id);
    setError(null);
    try {
      await api.delete(`/users/${target.id}`);
      setUsers((prev) => prev.filter((u) => u.id !== target.id));
    } catch (err) {
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to delete user.");
    } finally {
      setBusyId(null);
    }
  };

  if (authLoading || loading) {
    return (
      <div className="flex-1 flex items-center justify-center">
        <Loader2 className="w-6 h-6 animate-spin text-gray-400" strokeWidth={1.5} />
      </div>
    );
  }

  return (
    <main className="flex-1 max-w-3xl w-full mx-auto px-8 py-16 flex flex-col gap-8">
      <div>
        <h1 className="text-3xl font-semibold tracking-tight text-gray-900">User management</h1>
        <p className="text-gray-500 mt-2">Manage roles and access for all users.</p>
      </div>

      {error && (
        <p className="flex items-center gap-2 text-sm text-red-600">
          <AlertCircle className="w-4 h-4" strokeWidth={1.5} />
          {error}
        </p>
      )}

      {resetKeyInfo && (
        <div className="flex flex-col gap-2 rounded-2xl border border-blue-100 bg-blue-50 px-5 py-4">
          <div className="flex items-center justify-between">
            <p className="text-sm font-medium text-blue-900">
              Reset key for <span className="font-semibold">{resetKeyInfo.username}</span>
            </p>
            <button
              onClick={() => setResetKeyInfo(null)}
              aria-label="Dismiss"
              className="text-blue-400 hover:text-blue-700"
            >
              <X className="w-4 h-4" strokeWidth={1.5} />
            </button>
          </div>
          <div className="flex items-center gap-2">
            <code className="flex-1 text-sm bg-white border border-blue-200 rounded-lg px-3 py-2 break-all">
              {resetKeyInfo.reset_key}
            </code>
            <button
              onClick={() => navigator.clipboard.writeText(resetKeyInfo.reset_key)}
              aria-label="Copy reset key"
              className="p-2 rounded-lg border border-blue-200 text-blue-700 hover:bg-blue-100 transition-colors"
            >
              <Copy className="w-4 h-4" strokeWidth={1.5} />
            </button>
          </div>
          <p className="text-xs text-blue-700">
            Expires {new Date(resetKeyInfo.expires_at).toLocaleString()}. Share this key with the user
            through a trusted channel — it lets them set a new password on the reset-password page.
          </p>
        </div>
      )}

      <div className="flex flex-col">
        {users.map((u) => (
          <div
            key={u.id}
            className="flex items-center justify-between gap-4 py-4 border-b border-gray-100"
          >
            <div className="flex items-center gap-3 min-w-0">
              <div className={`p-2 rounded-full ${u.role === "admin" ? "bg-blue-50 text-blue-600" : "bg-gray-100 text-gray-500"}`}>
                {u.role === "admin" ? <ShieldCheck className="w-4 h-4" strokeWidth={1.5} /> : <UserIcon className="w-4 h-4" strokeWidth={1.5} />}
              </div>
              <div className="min-w-0">
                <p className="font-medium text-gray-900 truncate">
                  {u.username}
                  {u.id === user?.id && <span className="ml-2 text-xs text-gray-400">(you)</span>}
                </p>
                <p className="text-xs uppercase tracking-wide text-gray-400">{u.role}</p>
              </div>
            </div>

            <div className="flex items-center gap-2 shrink-0">
              {busyId === u.id ? (
                <Loader2 className="w-4 h-4 animate-spin text-gray-400" strokeWidth={1.5} />
              ) : (
                <>
                  <select
                    value={u.role}
                    onChange={(e) => changeRole(u, e.target.value as "admin" | "user")}
                    disabled={u.id === user?.id}
                    className="border border-gray-200 rounded-full px-3 py-1.5 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 disabled:opacity-50"
                  >
                    <option value="user">user</option>
                    <option value="admin">admin</option>
                  </select>
                  <button
                    onClick={() => issueResetKey(u)}
                    aria-label={`Issue reset key for ${u.username}`}
                    title="Issue password reset key"
                    className="p-2 rounded-full text-gray-400 hover:text-blue-600 hover:bg-blue-50 transition-colors"
                  >
                    <KeyRound className="w-4 h-4" strokeWidth={1.5} />
                  </button>
                  <button
                    onClick={() => deleteUser(u)}
                    disabled={u.id === user?.id}
                    aria-label={`Delete ${u.username}`}
                    className="p-2 rounded-full text-gray-400 hover:text-red-600 hover:bg-red-50 disabled:opacity-30 disabled:hover:bg-transparent disabled:hover:text-gray-400 transition-colors"
                  >
                    <Trash2 className="w-4 h-4" strokeWidth={1.5} />
                  </button>
                </>
              )}
            </div>
          </div>
        ))}
      </div>
    </main>
  );
}
