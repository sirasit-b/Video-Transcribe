"use client";

import { useState } from "react";
import Link from "next/link";
import axios from "axios";
import { KeyRound, Loader2, AlertCircle, CheckCircle2 } from "lucide-react";
import { api } from "../lib/api";

export default function ResetPasswordPage() {
  const [username, setUsername] = useState("");
  const [resetKey, setResetKey] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (password !== confirm) {
      setError("Passwords do not match.");
      return;
    }
    if (password.length < 6) {
      setError("Password must be at least 6 characters.");
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await api.post("/auth/reset-password", {
        username: username.trim(),
        reset_key: resetKey.trim(),
        new_password: password,
      });
      setSuccess(true);
    } catch (err) {
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to reset password. Please try again.");
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <main className="flex-1 flex items-center justify-center px-6 py-16">
      <div className="w-full max-w-sm flex flex-col gap-8">
        <div className="flex flex-col items-center gap-3 text-center">
          <div className="p-3 rounded-2xl bg-blue-50 text-blue-600">
            <KeyRound className="w-7 h-7" strokeWidth={1.5} />
          </div>
          <h1 className="text-2xl font-semibold tracking-tight text-gray-900">Reset password</h1>
          <p className="text-sm text-gray-500">
            Ask an admin for a reset key, then use it here to set a new password.
          </p>
        </div>

        {success ? (
          <div className="flex flex-col items-center gap-4 text-center">
            <p className="flex items-center gap-2 text-sm text-green-700">
              <CheckCircle2 className="w-4 h-4" strokeWidth={1.5} />
              Password reset successfully.
            </p>
            <Link
              href="/login"
              className="bg-blue-600 text-white px-5 py-2.5 rounded-full font-medium hover:bg-blue-700 transition-colors"
            >
              Sign in
            </Link>
          </div>
        ) : (
          <form onSubmit={handleSubmit} className="flex flex-col gap-4">
            <div className="flex flex-col gap-1.5">
              <label htmlFor="username" className="text-sm font-medium text-gray-700">Username</label>
              <input
                id="username"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                required
                autoComplete="username"
                className="border border-gray-200 rounded-xl px-4 py-2.5 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <label htmlFor="resetKey" className="text-sm font-medium text-gray-700">Reset key</label>
              <input
                id="resetKey"
                value={resetKey}
                onChange={(e) => setResetKey(e.target.value)}
                required
                className="border border-gray-200 rounded-xl px-4 py-2.5 text-sm font-mono focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <label htmlFor="password" className="text-sm font-medium text-gray-700">New password</label>
              <input
                id="password"
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
                autoComplete="new-password"
                className="border border-gray-200 rounded-xl px-4 py-2.5 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <label htmlFor="confirm" className="text-sm font-medium text-gray-700">Confirm new password</label>
              <input
                id="confirm"
                type="password"
                value={confirm}
                onChange={(e) => setConfirm(e.target.value)}
                required
                autoComplete="new-password"
                className="border border-gray-200 rounded-xl px-4 py-2.5 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
              />
            </div>

            {error && (
              <p className="flex items-center gap-2 text-sm text-red-600">
                <AlertCircle className="w-4 h-4" strokeWidth={1.5} />
                {error}
              </p>
            )}

            <button
              type="submit"
              disabled={submitting}
              className="bg-blue-600 text-white px-5 py-2.5 rounded-full font-medium hover:bg-blue-700 disabled:opacity-50 flex items-center justify-center gap-2 transition-colors"
            >
              {submitting ? <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} /> : <KeyRound className="w-4 h-4" strokeWidth={1.5} />}
              {submitting ? "Resetting..." : "Reset password"}
            </button>
          </form>
        )}

        <p className="text-center text-sm text-gray-500">
          Remembered your password?{" "}
          <Link href="/login" className="text-blue-600 hover:text-blue-700 font-medium">
            Sign in
          </Link>
        </p>
      </div>
    </main>
  );
}
