"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { Layers, Loader2, Upload, Film } from "lucide-react";
import { api } from "../lib/api";

interface SessionSummary {
  id: number;
  name: string;
  reference_video_id: number | null;
  created_at: string;
  videos: { id: number; original_name: string; trim_filename: string | null }[];
}

const VIDEO_EXTENSIONS = [
  "mp4", "mov", "m4v", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "mts", "m2ts", "ts", "3gp",
];

function isVideoFile(file: File): boolean {
  if (file.type.startsWith("video/")) return true;
  if (file.type) return false;
  return VIDEO_EXTENSIONS.includes(file.name.split(".").pop()?.toLowerCase() ?? "");
}

function dragHasFiles(event: DragEvent): boolean {
  const types = event.dataTransfer?.types;
  return types ? Array.from(types).includes("Files") : false;
}

export default function SessionsPage() {
  const router = useRouter();
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState<string | null>(null);
  const [isDraggingFiles, setIsDraggingFiles] = useState(false);
  const dragDepthRef = useRef(0);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const load = async () => {
    try {
      const res = await api.get<SessionSummary[]>("/sessions");
      setSessions(res.data);
    } catch (err) {
      console.error(err);
      setError("โหลดรายการเซสชันไม่สำเร็จ");
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    load();
  }, []);

  /** Make a session out of the dropped files and go to it.
   *
   *  The session is created first and the files are uploaded into it, so nothing
   *  has to be tidied up if an upload fails halfway: what arrived is already a
   *  session, and the rest can be dropped in after. */
  const createFromFiles = useCallback(
    async (files: File[]) => {
      const accepted = files.filter(isVideoFile);
      if (accepted.length === 0) {
        setError("ต้องเป็นไฟล์วิดีโอ");
        return;
      }
      setError(null);
      setCreating(`กำลังอัปโหลด ${accepted.length} คลิป…`);
      try {
        const created = await api.post<SessionSummary>("/sessions", {
          name: `เซสชัน ${new Date().toLocaleString("th-TH", {
            dateStyle: "short",
            timeStyle: "short",
          })}`,
        });
        const sessionId = created.data.id;
        for (const [index, file] of accepted.entries()) {
          setCreating(`กำลังอัปโหลด ${index + 1}/${accepted.length}: ${file.name}`);
          await api.post("/videos", file, {
            params: { filename: file.name, session_id: sessionId },
            headers: { "Content-Type": file.type || "application/octet-stream" },
          });
        }
        router.push(`/sessions/${sessionId}`);
      } catch (err) {
        console.error(err);
        setError("สร้างเซสชันไม่สำเร็จ");
        setCreating(null);
        load();
      }
    },
    [router]
  );

  useEffect(() => {
    const onDragEnter = (event: DragEvent) => {
      if (!dragHasFiles(event)) return;
      dragDepthRef.current += 1;
      setIsDraggingFiles(true);
    };
    const onDragOver = (event: DragEvent) => {
      if (!dragHasFiles(event)) return;
      event.preventDefault();
      if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
    };
    const onDragLeave = (event: DragEvent) => {
      if (!dragHasFiles(event)) return;
      dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
      if (dragDepthRef.current === 0) setIsDraggingFiles(false);
    };
    const onDrop = (event: DragEvent) => {
      if (!dragHasFiles(event)) return;
      event.preventDefault();
      dragDepthRef.current = 0;
      setIsDraggingFiles(false);
      createFromFiles(Array.from(event.dataTransfer?.files ?? []));
    };
    window.addEventListener("dragenter", onDragEnter);
    window.addEventListener("dragover", onDragOver);
    window.addEventListener("dragleave", onDragLeave);
    window.addEventListener("drop", onDrop);
    return () => {
      window.removeEventListener("dragenter", onDragEnter);
      window.removeEventListener("dragover", onDragOver);
      window.removeEventListener("dragleave", onDragLeave);
      window.removeEventListener("drop", onDrop);
    };
  }, [createFromFiles]);

  return (
    <main className="flex-1 max-w-3xl w-full mx-auto px-8 py-16 flex flex-col gap-8">
      {isDraggingFiles && (
        <div className="fixed inset-4 z-50 pointer-events-none rounded-3xl border-2 border-dashed border-blue-400 bg-blue-50/80 backdrop-blur-[2px] flex flex-col items-center justify-center gap-3">
          <Upload className="w-10 h-10 text-blue-600" strokeWidth={1.5} />
          <p className="text-lg font-medium text-blue-900">วางเพื่อสร้างเซสชันใหม่</p>
          <p className="text-sm text-blue-700">ทุกไฟล์ที่วางพร้อมกันจะถูกจับเป็นเซสชันเดียว</p>
        </div>
      )}

      <div className="flex items-start justify-between gap-6">
        <div>
          <h1 className="text-4xl font-semibold tracking-tight text-gray-900">เซสชันหลายกล้อง</h1>
          <p className="text-gray-500 mt-2 max-w-xl">
            ถ่ายคนไว้ตัวหนึ่ง อัดหน้าจอไว้อีกตัว (หรือหลายจอ) วางพร้อมกันที่นี่ได้เลย
            ระบบจะปรับระดับเสียงให้เท่ากัน ซิงค์ตามเสียงที่ได้ยินร่วมกัน แล้วตัดทุกคลิปให้ตรงกัน
          </p>
        </div>
        <button
          onClick={() => fileInputRef.current?.click()}
          disabled={creating !== null}
          className="shrink-0 bg-blue-600 text-white px-5 py-2.5 rounded-full font-medium hover:bg-blue-700 disabled:opacity-50 flex items-center gap-2 transition-colors"
        >
          {creating ? (
            <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
          ) : (
            <Layers className="w-4 h-4" strokeWidth={1.5} />
          )}
          เซสชันใหม่
        </button>
        <input
          ref={fileInputRef}
          type="file"
          accept="video/*"
          multiple
          className="hidden"
          onChange={(e) => {
            const files = Array.from(e.target.files ?? []);
            e.target.value = "";
            createFromFiles(files);
          }}
        />
      </div>

      {creating && <p className="text-sm text-gray-500">{creating}</p>}
      {error && <p className="text-sm text-amber-700">{error}</p>}

      {loading ? (
        <Loader2 className="w-5 h-5 animate-spin text-gray-400" strokeWidth={1.5} />
      ) : sessions.length === 0 ? (
        <button
          onClick={() => fileInputRef.current?.click()}
          className="rounded-2xl border-2 border-dashed border-gray-200 px-6 py-16 text-sm text-gray-500 hover:border-blue-300 hover:text-blue-700 transition-colors"
        >
          ยังไม่มีเซสชัน — วางไฟล์ของการถ่ายครั้งเดียวกันลงหน้านี้ หรือกดเพื่อเลือกไฟล์
        </button>
      ) : (
        <div className="flex flex-col">
          {sessions.map((session) => (
            <Link
              key={session.id}
              href={`/sessions/${session.id}`}
              className="flex items-center justify-between gap-4 py-4 border-b border-gray-100 hover:bg-gray-50 transition-colors -mx-4 px-4 rounded-lg"
            >
              <span className="flex items-center gap-2 min-w-0">
                <Film className="w-4 h-4 text-gray-300 shrink-0" strokeWidth={1.5} />
                <span className="truncate text-gray-900">{session.name}</span>
              </span>
              <span className="shrink-0 text-xs text-gray-400">
                {session.videos.length} คลิป
                {session.videos.every((v) => v.trim_filename) && session.videos.length > 0
                  ? " · ตัดแล้ว"
                  : ""}
              </span>
            </Link>
          ))}
        </div>
      )}
    </main>
  );
}
