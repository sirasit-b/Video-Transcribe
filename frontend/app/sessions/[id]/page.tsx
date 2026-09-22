"use client";

import { use, useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import axios from "axios";
import {
  AlertCircle,
  ArrowLeft,
  Check,
  CheckCircle2,
  Clock,
  Download,
  FileText,
  Link2,
  Loader2,
  Pencil,
  Scissors,
  Sparkles,
  Star,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { api, MEDIA_BASE, TOKEN_KEY } from "../../lib/api";
import SyncedPlayer, { Cue, PlayerTrack } from "./SyncedPlayer";

interface Waveform {
  buckets: number;
  before: number[];
  after: number[];
  kept?: number[];
}

interface SyncDrift {
  ppm: number;
  seconds_per_hour: number;
  significant: boolean;
}

interface MemberSync {
  offset_seconds: number;
  clearance: number | null;
  reliable: boolean;
  overlap_seconds: number;
  drift: SyncDrift | null;
  warning: string | null;
  role?: string;
  loudness_lufs?: number | null;
  gain_db?: number | null;
}

interface TranscribeModel {
  id: string;
  label: string;
  is_default: boolean;
}

interface SessionVideo {
  id: number;
  original_name: string;
  has_transcript: boolean;
  trim_filename: string | null;
  trim_result: (Record<string, unknown> & { waveform?: Waveform; output_duration?: number }) | null;
  sync_result: MemberSync | null;
}

interface SessionTrack {
  video_path: string;
  role: string;
  loudness_lufs: number | null;
  gain_db: number;
  sync: MemberSync | null;
}

interface SessionData {
  id: number;
  name: string;
  reference_video_id: number | null;
  job_id: string | null;
  result: { tracks?: SessionTrack[] } | null;
  videos: SessionVideo[];
}

interface SessionJob {
  job_id: string | null;
  mode?: "analyze" | "trim" | "sync";
  state: "running" | "canceling" | "done" | "failed" | "canceled" | "none" | "expired";
  phase?: string;
  progress?: number;
  elapsed_seconds?: number;
  eta_seconds?: number | null;
  error?: string | null;
}

const PHASE_LABELS: Record<string, string> = {
  queued: "เข้าคิว",
  analyzing: "ฟังเสียงทุกคลิป",
  rendering: "เรนเดอร์",
  finishing: "รวมไฟล์",
  finished: "เสร็จแล้ว",
};

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

function withAuthToken(url: string): string {
  const token = typeof window !== "undefined" ? localStorage.getItem(TOKEN_KEY) : null;
  if (!token) return url;
  return `${url}${url.includes("?") ? "&" : "?"}token=${encodeURIComponent(token)}`;
}

/** Save a response body under a name of our choosing.
 *
 *  Fetched rather than linked because the captions endpoint reads the token from
 *  the header, and an `<a href>` cannot send one.
 */
function saveBlob(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

function baseName(name: string): string {
  return name.replace(/\.[^./\\]+$/, "") || name;
}

function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "-";
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const m = Math.floor(seconds / 60);
  return `${m}m ${Math.round(seconds % 60)}s`;
}

export default function SessionPage({ params }: { params: Promise<{ id: string }> }) {
  const sessionId = use(params).id;
  const router = useRouter();

  const [session, setSession] = useState<SessionData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [job, setJob] = useState<SessionJob | null>(null);
  const [starting, setStarting] = useState<"sync" | "trim" | null>(null);
  const [uploads, setUploads] = useState<{ name: string; progress: number }[]>([]);
  const [isDraggingFiles, setIsDraggingFiles] = useState(false);
  const dragDepthRef = useRef(0);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const srtInputRef = useRef<HTMLInputElement>(null);
  const [srtTarget, setSrtTarget] = useState<number | null>(null);
  const [models, setModels] = useState<TranscribeModel[]>([]);
  const [model, setModel] = useState("");
  const [transcribing, setTranscribing] = useState<number | null>(null);
  const [nameDraft, setNameDraft] = useState<string | null>(null);
  const [savingName, setSavingName] = useState(false);
  // Whose transcript is shown with the playback: the one being listened to.
  const [readingIndex, setReadingIndex] = useState(0);
  const [cues, setCues] = useState<Cue[]>([]);

  const load = useCallback(async () => {
    try {
      const res = await api.get<SessionData>(`/sessions/${sessionId}`);
      setSession(res.data);
      setError(null);
    } catch (err) {
      console.error(err);
      setError("โหลดเซสชันไม่สำเร็จ");
    } finally {
      setLoading(false);
    }
  }, [sessionId]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    (async () => {
      try {
        const res = await api.get<TranscribeModel[]>("/transcribe-models");
        setModels(res.data);
        setModel(res.data.find((m) => m.is_default)?.id ?? res.data[0]?.id ?? "");
      } catch (err) {
        console.error(err);
      }
    })();
  }, []);

  /** Upload straight into the session, so a file dropped here is a member of it
   *  from the moment it lands rather than something to go and find afterwards. */
  const uploadFiles = useCallback(
    async (files: File[]) => {
      const accepted = files.filter(isVideoFile);
      const rejected = files.length - accepted.length;
      setError(rejected > 0 ? `ข้ามไฟล์ที่ไม่ใช่วิดีโอ ${rejected} ไฟล์` : null);
      if (accepted.length === 0) return;

      setUploads(accepted.map((file) => ({ name: file.name, progress: 0 })));
      for (const [index, file] of accepted.entries()) {
        try {
          await api.post("/videos", file, {
            params: { filename: file.name, session_id: Number(sessionId) },
            headers: { "Content-Type": file.type || "application/octet-stream" },
            onUploadProgress: (evt) => {
              const progress = evt.total ? Math.round((evt.loaded / evt.total) * 100) : 0;
              setUploads((prev) =>
                prev.map((u, i) => (i === index ? { ...u, progress } : u))
              );
            },
          });
        } catch (err) {
          console.error(err);
          setError(`อัปโหลด ${file.name} ไม่สำเร็จ`);
        }
      }
      setUploads([]);
      load();
    },
    [sessionId, load]
  );

  // Dropped anywhere on the page, and caught on the window so a file that misses
  // is ignored rather than opened by the browser over the top of this page.
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
      uploadFiles(Array.from(event.dataTransfer?.files ?? []));
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
  }, [uploadFiles]);

  const activeJobId =
    job && (job.state === "running" || job.state === "canceling")
      ? job.job_id
      : session?.job_id ?? null;
  const jobActive = activeJobId !== null;

  useEffect(() => {
    if (!activeJobId) return;
    let stopped = false;
    const poll = async () => {
      try {
        const res = await api.get<SessionJob>(`/sessions/${sessionId}/job`, {
          params: { job_id: activeJobId },
        });
        if (stopped) return;
        setJob(res.data);
        if (res.data.state === "failed") setError(res.data.error || "งานล้มเหลว");
        if (res.data.state === "done") load();
      } catch (err) {
        console.error(err);
        if (!stopped) setJob(null);
      }
    };
    poll();
    const timer = setInterval(poll, 1200);
    return () => {
      stopped = true;
      clearInterval(timer);
    };
  }, [activeJobId, sessionId, load]);

  const start = async (mode: "sync" | "trim") => {
    setStarting(mode);
    setError(null);
    try {
      const body = mode === "trim" ? { threshold: 0.04, margin_start: 0.2, margin_end: 0.2 } : {};
      const res = await api.post<SessionJob>(`/sessions/${sessionId}/${mode}`, body);
      setJob(res.data);
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || (mode === "sync" ? "ซิงค์ไม่สำเร็จ" : "ตัดไม่สำเร็จ"));
    } finally {
      setStarting(null);
    }
  };

  const cancel = async () => {
    try {
      await api.delete(`/sessions/${sessionId}/job`);
      setJob(null);
      load();
    } catch (err) {
      console.error(err);
    }
  };

  const removeVideo = async (videoId: number) => {
    try {
      await api.delete(`/sessions/${sessionId}/videos/${videoId}`);
      load();
    } catch (err) {
      console.error(err);
      setError("เอาคลิปออกจากเซสชันไม่สำเร็จ");
    }
  };

  const makeReference = async (videoId: number) => {
    try {
      const res = await api.post<SessionData>(`/sessions/${sessionId}/reference/${videoId}`);
      setSession(res.data);
    } catch (err) {
      console.error(err);
      setError("ตั้งคลิปอ้างอิงไม่สำเร็จ");
    }
  };

  const saveName = async () => {
    const name = (nameDraft ?? "").trim();
    if (!name || name === session?.name) {
      setNameDraft(null);
      return;
    }
    setSavingName(true);
    try {
      const res = await api.patch<SessionData>(`/sessions/${sessionId}`, { name });
      setSession(res.data);
      setNameDraft(null);
    } catch (err) {
      console.error(err);
      setError("เปลี่ยนชื่อเซสชันไม่สำเร็จ");
    } finally {
      setSavingName(false);
    }
  };

  /** Transcribe one recording. The source is transcribed, not the cut, so the
   *  transcript survives a re-trim; the cues are moved onto the cut's clock
   *  wherever they are shown or downloaded. */
  const transcribe = async (videoId: number) => {
    setTranscribing(videoId);
    setError(null);
    try {
      await api.post(`/videos/${videoId}/transcribe`, { model: model || null });
      await load();
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "ถอดเสียงไม่สำเร็จ");
    } finally {
      setTranscribing(null);
    }
  };

  const uploadSrt = async (videoId: number, file: File) => {
    const form = new FormData();
    form.append("file", file);
    try {
      await api.post(`/videos/${videoId}/captions`, form, {
        headers: { "Content-Type": "multipart/form-data" },
      });
      await load();
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "อัปโหลด SRT ไม่สำเร็จ");
    }
  };

  /** The transcript as an .srt, timed against the cut when there is one — the
   *  original timings would be minutes out against the trimmed file. */
  const downloadSrt = async (video: SessionVideo) => {
    try {
      const res = await api.get(`/videos/${video.id}/captions.srt`, {
        params: { timeline: video.trim_filename ? "trimmed" : "source" },
        responseType: "blob",
      });
      const suffix = video.trim_filename ? "_trimmed" : "";
      saveBlob(res.data as Blob, `${baseName(video.original_name)}${suffix}.srt`);
    } catch (err) {
      console.error(err);
      setError("ดาวน์โหลด SRT ไม่สำเร็จ");
    }
  };

  const deleteSession = async () => {
    if (!confirm("ลบเซสชันนี้? (ไฟล์วิดีโอและไฟล์ที่ตัดแล้วยังอยู่)")) return;
    try {
      await api.delete(`/sessions/${sessionId}`);
      router.push("/sessions");
    } catch (err) {
      console.error(err);
      setError("ลบเซสชันไม่สำเร็จ");
    }
  };

  // Whoever is being listened to is who is being read. Fetched already timed
  // against the cut, so the page never has to know what was taken out.
  const members = session?.videos ?? [];
  const reading = members[readingIndex] ?? members[0];
  const readingId = reading?.id ?? null;
  const readingHasCut = Boolean(reading?.trim_filename);
  const readingHasTranscript = Boolean(reading?.has_transcript);
  useEffect(() => {
    if (!readingId || !readingHasTranscript) {
      setCues([]);
      return;
    }
    let active = true;
    (async () => {
      try {
        const res = await api.get<{ segments: Cue[] }>(`/videos/${readingId}/captions.json`, {
          params: { timeline: readingHasCut ? "trimmed" : "source" },
        });
        if (active) setCues(res.data.segments ?? []);
      } catch (err) {
        console.error(err);
        if (active) setCues([]);
      }
    })();
    return () => {
      active = false;
    };
  }, [readingId, readingHasCut, readingHasTranscript]);

  if (loading) {
    return (
      <main className="flex-1 max-w-4xl w-full mx-auto px-8 py-16">
        <Loader2 className="w-5 h-5 animate-spin text-gray-400" strokeWidth={1.5} />
      </main>
    );
  }
  if (!session) {
    return (
      <main className="flex-1 max-w-4xl w-full mx-auto px-8 py-16 flex flex-col gap-4">
        <p className="text-sm text-gray-500">{error || "ไม่พบเซสชันนี้"}</p>
        <Link href="/sessions" className="text-sm text-blue-600 hover:text-blue-700">
          ← กลับไปหน้าเซสชัน
        </Link>
      </main>
    );
  }

  const synced = members.length > 1 && members.every((v) => v.sync_result);
  const cut = members.length > 1 && members.every((v) => v.trim_filename);
  const playerTracks: PlayerTrack[] = members.map((video) => ({
    id: video.id,
    name: video.original_name,
    duration: Number(video.trim_result?.output_duration ?? 0),
    waveform: video.trim_result?.waveform?.after ?? [],
    role: video.id === session.reference_video_id ? "reference" : "other",
  }));

  return (
    <main className="flex-1 max-w-4xl w-full mx-auto px-8 py-12 flex flex-col gap-8">
      {isDraggingFiles && (
        <div className="fixed inset-4 z-50 pointer-events-none rounded-3xl border-2 border-dashed border-blue-400 bg-blue-50/80 backdrop-blur-[2px] flex flex-col items-center justify-center gap-3">
          <Upload className="w-10 h-10 text-blue-600" strokeWidth={1.5} />
          <p className="text-lg font-medium text-blue-900">วางเพื่อเพิ่มเข้าเซสชันนี้</p>
        </div>
      )}

      <div className="flex items-start justify-between gap-4">
        <div>
          <Link
            href="/sessions"
            className="inline-flex items-center gap-1 text-sm text-gray-500 hover:text-gray-700 mb-2"
          >
            <ArrowLeft className="w-4 h-4" strokeWidth={1.5} />
            เซสชันทั้งหมด
          </Link>
          {nameDraft === null ? (
            <button
              onClick={() => setNameDraft(session.name)}
              title="เปลี่ยนชื่อเซสชัน"
              className="group flex items-center gap-2 text-left"
            >
              <h1 className="text-3xl font-semibold tracking-tight text-gray-900">
                {session.name}
              </h1>
              <Pencil
                className="w-4 h-4 text-gray-300 group-hover:text-gray-500 transition-colors"
                strokeWidth={1.5}
              />
            </button>
          ) : (
            <div className="flex items-center gap-2">
              <input
                autoFocus
                value={nameDraft}
                onChange={(e) => setNameDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") saveName();
                  if (e.key === "Escape") setNameDraft(null);
                }}
                className="text-3xl font-semibold tracking-tight text-gray-900 border-b border-gray-300 focus:border-blue-500 focus:outline-none bg-transparent"
              />
              <button
                onClick={saveName}
                disabled={savingName}
                className="p-1.5 text-gray-400 hover:text-emerald-600 disabled:opacity-50"
                aria-label="บันทึกชื่อ"
              >
                {savingName ? (
                  <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
                ) : (
                  <Check className="w-4 h-4" strokeWidth={1.5} />
                )}
              </button>
              <button
                onClick={() => setNameDraft(null)}
                className="p-1.5 text-gray-400 hover:text-gray-700"
                aria-label="ยกเลิก"
              >
                <X className="w-4 h-4" strokeWidth={1.5} />
              </button>
            </div>
          )}
          <p className="text-sm text-gray-500 mt-1 max-w-2xl">
            วางไฟล์ของเซสชันเดียวกันลงหน้านี้ได้เลย ระบบจะปรับระดับเสียงให้เท่ากันก่อน
            แล้วหาว่าแต่ละคลิปเริ่มห่างกันกี่วินาทีจากเสียงที่ไมค์ทุกตัวได้ยินร่วมกัน
            จากนั้นจึงตัดทุกคลิปบนไทม์ไลน์เดียวกัน
          </p>
        </div>
        <button
          onClick={deleteSession}
          className="shrink-0 text-gray-400 hover:text-red-600 p-2"
          title="ลบเซสชัน"
        >
          <Trash2 className="w-4 h-4" strokeWidth={1.5} />
        </button>
      </div>

      {error && (
        <p className="flex items-start gap-2 text-sm text-amber-700">
          <AlertCircle className="w-4 h-4 mt-0.5 shrink-0" strokeWidth={1.5} />
          {error}
        </p>
      )}

      {/* The recordings */}
      <div className="flex flex-col gap-2">
        {members.map((video, index) => {
          const isReference = video.id === session.reference_video_id;
          const sync = video.sync_result;
          return (
            <div
              key={video.id}
              className="flex items-center gap-3 rounded-xl border border-gray-200 px-4 py-3"
            >
              <span className="w-5 shrink-0 text-xs text-gray-400 tabular-nums">{index + 1}</span>
              <div className="flex-1 min-w-0">
                <Link
                  href={`/videos/${video.id}`}
                  className="text-sm text-gray-900 hover:text-blue-700 truncate block"
                >
                  {video.original_name}
                </Link>
                <p className="text-[11px] text-gray-500 mt-0.5 flex flex-wrap gap-x-3">
                  {isReference ? (
                    <span className="text-amber-700">คลิปอ้างอิงเวลา</span>
                  ) : sync ? (
                    <span>
                      เริ่ม{sync.offset_seconds >= 0 ? "ช้ากว่า" : "เร็วกว่า"}{" "}
                      {Math.abs(sync.offset_seconds).toFixed(3)} วินาที
                    </span>
                  ) : (
                    <span className="text-gray-400">ยังไม่ได้ซิงค์</span>
                  )}
                  {sync?.loudness_lufs != null && (
                    <span>
                      {sync.loudness_lufs.toFixed(1)} LUFS
                      {sync.gain_db != null && Math.abs(sync.gain_db) >= 0.1
                        ? ` → ปรับ ${sync.gain_db > 0 ? "+" : ""}${sync.gain_db.toFixed(1)} dB`
                        : ""}
                    </span>
                  )}
                  {sync?.drift?.significant && (
                    <span className="text-amber-700">
                      นาฬิกาต่างกัน {Math.abs(sync.drift.ppm).toFixed(0)} ppm (ชดเชยให้แล้ว)
                    </span>
                  )}
                  {video.trim_filename && <span className="text-emerald-700">ตัดแล้ว</span>}
                </p>
              </div>
              {/* Everything you would otherwise leave the session to do. */}
              {video.trim_filename && (
                <a
                  href={withAuthToken(
                    `${MEDIA_BASE}/api/videos/${video.id}/trimmed?download=true`
                  )}
                  title="ดาวน์โหลดไฟล์ที่ตัดแล้ว"
                  className="shrink-0 p-1.5 text-gray-400 hover:text-blue-600"
                >
                  <Download className="w-4 h-4" strokeWidth={1.5} />
                </a>
              )}
              <button
                onClick={() => transcribe(video.id)}
                disabled={transcribing !== null || jobActive}
                title={video.has_transcript ? "ถอดเสียงใหม่" : "ถอดเสียงคลิปนี้"}
                className={`shrink-0 p-1.5 disabled:opacity-40 ${
                  video.has_transcript
                    ? "text-emerald-600 hover:text-emerald-700"
                    : "text-gray-300 hover:text-gray-600"
                }`}
              >
                {transcribing === video.id ? (
                  <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
                ) : (
                  <Sparkles className="w-4 h-4" strokeWidth={1.5} />
                )}
              </button>
              <button
                onClick={() => {
                  setSrtTarget(video.id);
                  srtInputRef.current?.click();
                }}
                title="อัปโหลด SRT ให้คลิปนี้"
                className="shrink-0 p-1.5 text-gray-300 hover:text-gray-600"
              >
                <Upload className="w-4 h-4" strokeWidth={1.5} />
              </button>
              {video.has_transcript && (
                <button
                  onClick={() => downloadSrt(video)}
                  title={
                    video.trim_filename
                      ? "ดาวน์โหลด SRT (เวลาตรงกับไฟล์ที่ตัดแล้ว)"
                      : "ดาวน์โหลด SRT"
                  }
                  className="shrink-0 p-1.5 text-gray-300 hover:text-gray-600"
                >
                  <FileText className="w-4 h-4" strokeWidth={1.5} />
                </button>
              )}
              {!isReference && (
                <button
                  onClick={() => makeReference(video.id)}
                  disabled={jobActive}
                  title="ใช้คลิปนี้เป็นตัวอ้างอิงเวลา"
                  className="shrink-0 p-1.5 text-gray-300 hover:text-amber-500 disabled:opacity-40"
                >
                  <Star className="w-4 h-4" strokeWidth={1.5} />
                </button>
              )}
              <button
                onClick={() => removeVideo(video.id)}
                disabled={jobActive}
                title="เอาออกจากเซสชัน (ไม่ลบไฟล์)"
                className="shrink-0 p-1.5 text-gray-300 hover:text-gray-600 disabled:opacity-40"
              >
                <X className="w-4 h-4" strokeWidth={1.5} />
              </button>
            </div>
          );
        })}

        {uploads.map((upload) => (
          <div
            key={upload.name}
            className="flex items-center gap-3 rounded-xl border border-gray-100 bg-gray-50/60 px-4 py-3 text-sm"
          >
            <span className="flex-1 min-w-0 truncate text-gray-600">{upload.name}</span>
            <div className="w-28 shrink-0 h-1.5 rounded-full bg-gray-200 overflow-hidden">
              <div
                className="h-full bg-blue-600 transition-all"
                style={{ width: `${upload.progress}%` }}
              />
            </div>
          </div>
        ))}

        {members.length === 0 && uploads.length === 0 && (
          <button
            onClick={() => fileInputRef.current?.click()}
            className="rounded-xl border-2 border-dashed border-gray-200 px-6 py-10 text-sm text-gray-500 hover:border-blue-300 hover:text-blue-700 transition-colors"
          >
            วางไฟล์ของเซสชันนี้ที่นี่ หรือกดเพื่อเลือกไฟล์
          </button>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-3">
        <button
          onClick={() => fileInputRef.current?.click()}
          className="border border-gray-300 text-gray-700 px-4 py-2 rounded-full text-sm font-medium hover:bg-gray-50 flex items-center gap-2 transition-colors"
        >
          <Upload className="w-4 h-4" strokeWidth={1.5} />
          เพิ่มคลิป
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
            uploadFiles(files);
          }}
        />
        <select
          value={model}
          onChange={(e) => setModel(e.target.value)}
          aria-label="โมเดลถอดเสียง"
          title="โมเดลที่ใช้เมื่อกดถอดเสียงในรายการด้านบน"
          className="border border-gray-200 rounded-full px-4 py-2 text-sm text-gray-700 bg-white focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
        >
          {models.map((m) => (
            <option key={m.id} value={m.id}>
              {m.label}
            </option>
          ))}
        </select>
        <input
          ref={srtInputRef}
          type="file"
          accept=".srt,text/plain"
          className="hidden"
          onChange={(e) => {
            const file = e.target.files?.[0];
            e.target.value = "";
            if (file && srtTarget !== null) uploadSrt(srtTarget, file);
            setSrtTarget(null);
          }}
        />
        <button
          onClick={() => start("sync")}
          disabled={members.length < 2 || jobActive || starting !== null}
          className="border border-gray-300 text-gray-700 px-4 py-2 rounded-full text-sm font-medium hover:bg-gray-50 disabled:opacity-50 flex items-center gap-2 transition-colors"
        >
          {starting === "sync" ? (
            <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
          ) : (
            <Link2 className="w-4 h-4" strokeWidth={1.5} />
          )}
          ปรับระดับเสียง + ซิงค์
        </button>
        <button
          onClick={() => start("trim")}
          disabled={members.length < 2 || jobActive || starting !== null}
          title="ตัดช่วงเงียบออกจากทุกคลิปบนไทม์ไลน์เดียวกัน"
          className="bg-blue-600 text-white px-5 py-2 rounded-full text-sm font-medium hover:bg-blue-700 disabled:opacity-50 flex items-center gap-2 transition-colors"
        >
          {starting === "trim" ? (
            <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
          ) : (
            <Scissors className="w-4 h-4" strokeWidth={1.5} />
          )}
          {cut ? "ตัดใหม่ทั้งหมด" : "ตัดทั้งหมด"}
        </button>
        {synced && !cut && (
          <span className="flex items-center gap-1.5 text-xs text-emerald-700">
            <CheckCircle2 className="w-3.5 h-3.5" strokeWidth={1.5} />
            ซิงค์แล้ว พร้อมตัด
          </span>
        )}
      </div>

      {jobActive && job && (
        <div className="flex flex-col gap-2">
          <div className="flex items-center justify-between text-xs text-gray-500">
            <span>
              {job.state === "canceling"
                ? "กำลังยกเลิก…"
                : PHASE_LABELS[job.phase ?? ""] ?? "กำลังทำงาน"}{" "}
              · {Math.round((job.progress ?? 0) * 100)}%
            </span>
            <span className="flex items-center gap-3">
              <span className="flex items-center gap-1">
                <Clock className="w-3.5 h-3.5" strokeWidth={1.5} />
                ผ่านไป {formatDuration(job.elapsed_seconds ?? 0)}
                {job.eta_seconds != null ? ` · เหลือ ~${formatDuration(job.eta_seconds)}` : ""}
              </span>
              <button onClick={cancel} className="text-gray-400 hover:text-red-600">
                ยกเลิก
              </button>
            </span>
          </div>
          <div className="h-1.5 rounded-full bg-gray-100 overflow-hidden">
            <div
              className="h-full bg-blue-600 transition-all"
              style={{ width: `${Math.max(2, Math.round((job.progress ?? 0) * 100))}%` }}
            />
          </div>
        </div>
      )}

      {cut && !jobActive && (
        <div className="flex flex-col gap-3 pt-4 border-t border-gray-100">
          <div>
            <h2 className="text-lg font-semibold text-gray-900">ดูผลลัพธ์พร้อมกัน</h2>
            <p className="text-sm text-gray-500 mt-1">
              กดเล่นครั้งเดียว ทุกคลิปเดินพร้อมกัน เส้นเสียงวางเป็นชั้น ๆ บนแกนเวลาเดียวกัน
              ถ้าตรงกันจริง จังหวะเงียบและจังหวะพูดของทุกเส้นจะอยู่ตรงกันพอดี
              {cues.length > 0
                ? " ข้อความด้านล่างจะไล่ไฟตามที่กำลังพูด กดที่บรรทัดไหนก็กระโดดไปตรงนั้น"
                : " ถอดเสียงคลิปไหนไว้ ข้อความจะขึ้นมาไล่ตามเสียงที่กำลังฟังอยู่"}
            </p>
          </div>
          <SyncedPlayer
            tracks={playerTracks}
            cues={cues}
            onAudibleChange={setReadingIndex}
          />
        </div>
      )}
    </main>
  );
}
