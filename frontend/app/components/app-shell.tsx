"use client";

import { usePathname, useRouter } from "next/navigation";
import Link from "next/link";
import { useEffect } from "react";
import { Video, LogOut, Users, LayoutDashboard, Loader2 } from "lucide-react";
import { useAuth } from "../lib/auth-context";

const PUBLIC_ROUTES = ["/login", "/register", "/reset-password"];

export function AppShell({ children }: { children: React.ReactNode }) {
  const { user, loading, logout } = useAuth();
  const pathname = usePathname();
  const router = useRouter();

  const isPublic = PUBLIC_ROUTES.some((r) => pathname.startsWith(r));

  useEffect(() => {
    if (!loading && !user && !isPublic) {
      router.replace("/login");
    }
    if (!loading && user && isPublic) {
      router.replace("/");
    }
  }, [loading, user, isPublic, router]);

  if (loading) {
    return (
      <div className="min-h-screen flex items-center justify-center">
        <Loader2 className="w-6 h-6 animate-spin text-gray-400" strokeWidth={1.5} />
      </div>
    );
  }

  if (isPublic) {
    return <>{children}</>;
  }

  if (!user) {
    return (
      <div className="min-h-screen flex items-center justify-center">
        <Loader2 className="w-6 h-6 animate-spin text-gray-400" strokeWidth={1.5} />
      </div>
    );
  }

  return (
    <>
      <header className="border-b border-gray-100 px-8 py-5 flex items-center justify-between sticky top-0 z-20 bg-white/90 backdrop-blur">
        <Link href="/" className="flex items-center gap-2 text-gray-900 hover:text-gray-600 transition-colors">
          <Video className="w-6 h-6" strokeWidth={1.5} />
          <span className="text-lg font-semibold tracking-tight">Video Transcribe</span>
        </Link>
        <div className="flex items-center gap-3 text-sm">
          {/* ลิงก์ "ระบบคำ" ถูกซ่อนออกจากเมนูนี้ตามคำขอ — หน้า /glossary ยังใช้งานได้
              ตามปกติผ่านลิงก์อื่นในแอป (เช่นในรายงานการขัดคำ) หรือเข้า URL ตรง ๆ */}
          {user.role === "admin" && (
            <>
              <Link
                href="/admin"
                className={`flex items-center gap-1.5 px-3 py-1.5 rounded-full transition-colors ${
                  pathname === "/admin"
                    ? "bg-gray-100 text-gray-900"
                    : "text-gray-500 hover:bg-gray-50 hover:text-gray-900"
                }`}
              >
                <LayoutDashboard className="w-4 h-4" strokeWidth={1.5} />
                Overview
              </Link>
              <Link
                href="/admin/users"
                className={`flex items-center gap-1.5 px-3 py-1.5 rounded-full transition-colors ${
                  pathname.startsWith("/admin/users")
                    ? "bg-gray-100 text-gray-900"
                    : "text-gray-500 hover:bg-gray-50 hover:text-gray-900"
                }`}
              >
                <Users className="w-4 h-4" strokeWidth={1.5} />
                Users
              </Link>
            </>
          )}
          <span className="text-gray-500">
            {user.username}
            <span className="ml-1.5 text-xs uppercase tracking-wide text-gray-400">{user.role}</span>
          </span>
          <button
            onClick={logout}
            className="flex items-center gap-1.5 px-3 py-1.5 rounded-full text-gray-500 hover:bg-gray-50 hover:text-gray-900 transition-colors"
            aria-label="Log out"
          >
            <LogOut className="w-4 h-4" strokeWidth={1.5} />
            Logout
          </button>
        </div>
      </header>
      {children}
    </>
  );
}
