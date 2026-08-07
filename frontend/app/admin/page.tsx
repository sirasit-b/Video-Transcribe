"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { Loader2, AlertCircle, Users, Video as VideoIcon, FileText, HardDrive } from "lucide-react";
import { api } from "../lib/api";
import { useAuth } from "../lib/auth-context";

interface AdminVideoItem {
  id: number;
  filename: string;
  original_name: string;
  owner_username: string | null;
  has_transcript: boolean;
  size_bytes: number;
  created_at: string;
  is_deleted: boolean;
}

interface AdminStats {
  total_users: number;
  total_videos: number;
  transcribed_videos: number;
  total_storage_bytes: number;
}

interface AdminOverview {
  stats: AdminStats;
  videos: AdminVideoItem[];
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function StatCard({ icon, label, value }: { icon: React.ReactNode; label: string; value: string }) {
  return (
    <div className="flex items-center gap-4 border border-gray-100 rounded-2xl px-5 py-4">
      <div className="p-2.5 rounded-xl bg-blue-50 text-blue-600">{icon}</div>
      <div>
        <p className="text-2xl font-semibold tracking-tight text-gray-900">{value}</p>
        <p className="text-sm text-gray-500">{label}</p>
      </div>
    </div>
  );
}

export default function AdminOverviewPage() {
  const { user, loading: authLoading } = useAuth();
  const router = useRouter();

  const [data, setData] = useState<AdminOverview | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

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
        const res = await api.get<AdminOverview>("/admin/overview");
        if (active) setData(res.data);
      } catch (err) {
        console.error(err);
        if (active) setError("Failed to load system overview.");
      } finally {
        if (active) setLoading(false);
      }
    })();
    return () => {
      active = false;
    };
  }, [user]);

  if (authLoading || loading) {
    return (
      <div className="flex-1 flex items-center justify-center">
        <Loader2 className="w-6 h-6 animate-spin text-gray-400" strokeWidth={1.5} />
      </div>
    );
  }

  return (
    <main className="flex-1 max-w-5xl w-full mx-auto px-8 py-16 flex flex-col gap-8">
      <div>
        <h1 className="text-3xl font-semibold tracking-tight text-gray-900">System overview</h1>
        <p className="text-gray-500 mt-2">Usage across every account in the system.</p>
      </div>

      {error && (
        <p className="flex items-center gap-2 text-sm text-red-600">
          <AlertCircle className="w-4 h-4" strokeWidth={1.5} />
          {error}
        </p>
      )}

      {data && (
        <>
          <div className="grid grid-cols-2 sm:grid-cols-4 gap-4">
            <StatCard icon={<Users className="w-5 h-5" strokeWidth={1.5} />} label="Users" value={String(data.stats.total_users)} />
            <StatCard icon={<VideoIcon className="w-5 h-5" strokeWidth={1.5} />} label="Videos" value={String(data.stats.total_videos)} />
            <StatCard
              icon={<FileText className="w-5 h-5" strokeWidth={1.5} />}
              label="Transcribed"
              value={String(data.stats.transcribed_videos)}
            />
            <StatCard
              icon={<HardDrive className="w-5 h-5" strokeWidth={1.5} />}
              label="Storage used"
              value={formatBytes(data.stats.total_storage_bytes)}
            />
          </div>

          <div className="flex flex-col">
            <h2 className="text-lg font-medium text-gray-900 mb-3">All videos</h2>
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="text-left text-gray-400 border-b border-gray-100">
                    <th className="py-2 pr-4 font-medium">Name</th>
                    <th className="py-2 pr-4 font-medium">Owner</th>
                    <th className="py-2 pr-4 font-medium">Transcript</th>
                    <th className="py-2 pr-4 font-medium">Size</th>
                    <th className="py-2 pr-4 font-medium">Uploaded</th>
                  </tr>
                </thead>
                <tbody>
                  {data.videos.map((v) => (
                    <tr key={v.id} className={`border-b border-gray-50 ${v.is_deleted ? "opacity-50" : ""}`}>
                      <td className="py-3 pr-4 text-gray-900 max-w-xs truncate">
                        {v.original_name}
                        {v.is_deleted && (
                          <span className="ml-2 text-xs uppercase tracking-wide text-red-500">Deleted</span>
                        )}
                      </td>
                      <td className="py-3 pr-4 text-gray-500">{v.owner_username ?? "—"}</td>
                      <td className="py-3 pr-4 text-gray-500">{v.has_transcript ? "Yes" : "No"}</td>
                      <td className="py-3 pr-4 text-gray-500">{formatBytes(v.size_bytes)}</td>
                      <td className="py-3 pr-4 text-gray-500">{new Date(v.created_at).toLocaleDateString()}</td>
                    </tr>
                  ))}
                  {data.videos.length === 0 && (
                    <tr>
                      <td colSpan={5} className="py-6 text-center text-gray-400">
                        No videos uploaded yet.
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>
          </div>
        </>
      )}
    </main>
  );
}
