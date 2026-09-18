"use client";

import { useEffect, useRef, useState, use } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import axios from "axios";
import {
  ArrowLeft,
  FileText,
  Loader2,
  AlertCircle,
  Camera,
  ChevronLeft,
  ChevronRight,
  Download,
  X,
  Pencil,
  Check,
  Sparkles,
  Wand2,
  Clock,
  DollarSign,
  CheckSquare,
  Square,
  Archive,
  Trash2,
  Upload,
  Search,
  Replace,
  CaseSensitive,
  ChevronUp,
  ChevronDown,
} from "lucide-react";
import { api, MEDIA_BASE, TOKEN_KEY } from "../../lib/api";

interface CaptionSegment {
  start: number;
  end: number;
  text: string;
}

interface RunStats {
  operation?: string;
  model: string;
  audio_seconds?: number;
  elapsed_seconds: number;
  estimated_cost_usd: number;
  cost_basis: string;
  is_estimate_only?: boolean;
  requests?: number;
  realtime_factor?: number;
  input_tokens?: number;
  output_tokens?: number;
}

interface Correction {
  before: string;
  after: string;
  category: string;
  category_label: string;
  rule_id: string;
}

interface RuleSummary {
  rule_id: string;
  after: string;
  category: string;
  category_label: string;
  count: number;
  variants: [string, number][];
}

interface PolishReport {
  segments_in: number;
  segments_out: number;
  correction_count: number;
  by_category: Record<string, number>;
  by_rule: RuleSummary[];
  corrections: { index: number; before: string; after: string; corrections: Correction[] }[];
  timing_issues: { kind: string; text: string }[];
  line_split_count: number;
  tokenizer: string;
  line_budget?: { min: number; max: number };
}

interface CaptionReplacement {
  id: number;
  find_text: string;
  replace_text: string;
  match_case: boolean;
  occurrences: number;
  segments_changed: number;
  segment_indexes: number[];
  glossary_rule_id: string | null;
  created_at: string;
}

interface CaptionReplaceResult {
  video: VideoData;
  occurrences: number;
  segments_changed: number;
  glossary_rule_id: string | null;
  history: CaptionReplacement;
}

interface TranscribeEstimate {
  model: string;
  audio_seconds: number;
  estimated_seconds: number;
  estimated_cost_usd: number;
  basis: string;
  is_estimate_only: boolean;
}

interface VideoData {
  id: number;
  filename: string;
  original_name: string;
  has_transcript: boolean;
  transcript_text: string | null;
  caption_segments: CaptionSegment[] | null;
  transcribe_stats: RunStats | null;
  polish_report: PolishReport | null;
  rewrite_stats: RunStats | null;
}

interface TranscribeModel {
  id: string;
  label: string;
  description: string;
  provider: string;
  is_default: boolean;
}

const PROVIDER_LABELS: Record<string, string> = {
  openai: "OpenAI",
};

// Same palette as the glossary page, so a category reads the same in both places.
const CATEGORY_STYLES: Record<string, string> = {
  symbol: "bg-rose-50 text-rose-700 border-rose-200",
  ui: "bg-orange-50 text-orange-700 border-orange-200",
  filename: "bg-amber-50 text-amber-700 border-amber-200",
  path: "bg-lime-50 text-lime-700 border-lime-200",
  platform: "bg-sky-50 text-sky-700 border-sky-200",
  term: "bg-violet-50 text-violet-700 border-violet-200",
  english: "bg-blue-50 text-blue-700 border-blue-200",
};

function categoryClass(category: string): string {
  return CATEGORY_STYLES[category] ?? "bg-gray-50 text-gray-600 border-gray-200";
}

function formatDuration(seconds: number): string {
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const m = Math.floor(seconds / 60);
  const s = Math.round(seconds % 60);
  return `${m}m ${s.toString().padStart(2, "0")}s`;
}

function formatCost(usd: number): string {
  // Runs are routinely well under a cent, so a flat 2dp would show "$0.00".
  if (usd === 0) return "$0";
  if (usd < 0.01) return `$${usd.toFixed(4)}`;
  return `$${usd.toFixed(2)}`;
}

/** Split a caption line so the words the word system replaced can be marked. */
function highlightSegments(text: string, corrections: Correction[]): { text: string; category?: string }[] {
  if (!corrections.length) return [{ text }];

  // Longest replacement first: "Roboflow Universe" must win over "Roboflow".
  const terms = Array.from(new Set(corrections.map((c) => c.after)))
    .filter(Boolean)
    .sort((a, b) => b.length - a.length);
  const categoryOf = new Map(corrections.map((c) => [c.after, c.category]));

  const parts: { text: string; category?: string }[] = [];
  let cursor = 0;
  while (cursor < text.length) {
    let matched: string | null = null;
    for (const term of terms) {
      if (text.startsWith(term, cursor)) {
        matched = term;
        break;
      }
    }
    if (matched) {
      parts.push({ text: matched, category: categoryOf.get(matched) });
      cursor += matched.length;
    } else {
      const last = parts[parts.length - 1];
      if (last && last.category === undefined) last.text += text[cursor];
      else parts.push({ text: text[cursor] });
      cursor += 1;
    }
  }
  return parts;
}

/** Split a caption line on every occurrence of the searched text. */
function splitOnMatches(text: string, find: string, matchCase: boolean): { text: string; isMatch: boolean }[] {
  if (!find) return [{ text, isMatch: false }];

  const haystack = matchCase ? text : text.toLowerCase();
  const needle = matchCase ? find : find.toLowerCase();

  const parts: { text: string; isMatch: boolean }[] = [];
  let cursor = 0;
  let found = haystack.indexOf(needle);
  while (found !== -1) {
    if (found > cursor) parts.push({ text: text.slice(cursor, found), isMatch: false });
    // Slice from the original text, so the match keeps its real casing.
    parts.push({ text: text.slice(found, found + find.length), isMatch: true });
    cursor = found + find.length;
    found = haystack.indexOf(needle, cursor);
  }
  if (cursor < text.length) parts.push({ text: text.slice(cursor), isMatch: false });
  return parts;
}

function countMatches(text: string, find: string, matchCase: boolean): number {
  if (!find) return 0;
  const haystack = matchCase ? text : text.toLowerCase();
  const needle = matchCase ? find : find.toLowerCase();
  let count = 0;
  let cursor = haystack.indexOf(needle);
  while (cursor !== -1) {
    count += 1;
    cursor = haystack.indexOf(needle, cursor + needle.length);
  }
  return count;
}

interface FrameItem {
  filename: string;
  url: string;
}

const MODEL_KEY = "vt_transcribe_model";

function formatTimestamp(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  return `${m}:${s.toString().padStart(2, "0")}`;
}

function withAuthToken(url: string): string {
  const token = typeof window !== "undefined" ? localStorage.getItem(TOKEN_KEY) : null;
  if (!token) return url;
  return `${url}${url.includes("?") ? "&" : "?"}token=${encodeURIComponent(token)}`;
}

function triggerBlobDownload(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

export default function VideoPage({ params }: { params: Promise<{ id: string }> }) {
  const resolvedParams = use(params);
  const videoId = resolvedParams.id;
  const router = useRouter();

  const [video, setVideo] = useState<VideoData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const [isTranscribing, setIsTranscribing] = useState(false);
  const [transcribeModels, setTranscribeModels] = useState<TranscribeModel[]>([]);
  const [selectedModel, setSelectedModel] = useState("");
  const [isRewriting, setIsRewriting] = useState(false);
  const [isPolishing, setIsPolishing] = useState(false);
  const [estimate, setEstimate] = useState<TranscribeEstimate | null>(null);
  const [showReport, setShowReport] = useState(false);
  const [isUploadingSrt, setIsUploadingSrt] = useState(false);
  const [frameCount, setFrameCount] = useState("6");
  const [isExtractingFrames, setIsExtractingFrames] = useState(false);
  const [frames, setFrames] = useState<FrameItem[]>([]);
  const [selectedFrames, setSelectedFrames] = useState<Set<string>>(new Set());
  const [isDownloadingAll, setIsDownloadingAll] = useState(false);
  const [isDownloadingSelected, setIsDownloadingSelected] = useState(false);
  const [activeFrameIndex, setActiveFrameIndex] = useState<number | null>(null);

  const [isRenaming, setIsRenaming] = useState(false);
  const [nameDraft, setNameDraft] = useState("");
  const [isSavingName, setIsSavingName] = useState(false);
  const [isDeleting, setIsDeleting] = useState(false);

  const videoRef = useRef<HTMLVideoElement>(null);
  const srtInputRef = useRef<HTMLInputElement>(null);
  const cueRefs = useRef<(HTMLDivElement | null)[]>([]);
  // Whether the video was actually playing when a cue edit started, so finishing the
  // edit only resumes playback if it had interrupted it.
  const wasPlayingRef = useRef(false);
  const [activeCueIndex, setActiveCueIndex] = useState<number | null>(null);
  const [editingCueIndex, setEditingCueIndex] = useState<number | null>(null);
  const [cueDraft, setCueDraft] = useState("");
  const [savingCueIndex, setSavingCueIndex] = useState<number | null>(null);
  const [captionsVersion, setCaptionsVersion] = useState(0);

  const findInputRef = useRef<HTMLInputElement>(null);
  const [showFindReplace, setShowFindReplace] = useState(false);
  const [findText, setFindText] = useState("");
  const [replaceText, setReplaceText] = useState("");
  const [matchCase, setMatchCase] = useState(false);
  const [saveToGlossary, setSaveToGlossary] = useState(true);
  const [replacingScope, setReplacingScope] = useState<"all" | number | null>(null);
  const [replaceNotice, setReplaceNotice] = useState<string | null>(null);
  // Which hit of the search is currently scrolled to, as an offset into matchedIndexes.
  const [matchCursor, setMatchCursor] = useState(0);

  const captionSegments = video?.caption_segments ?? [];
  const matchedIndexes = findText
    ? captionSegments.reduce<number[]>((acc, seg, i) => {
        if (countMatches(seg.text, findText, matchCase) > 0) acc.push(i);
        return acc;
      }, [])
    : [];
  const totalMatches = findText
    ? captionSegments.reduce((sum, seg) => sum + countMatches(seg.text, findText, matchCase), 0)
    : 0;

  const handleTranscribe = async () => {
    setIsTranscribing(true);
    setError(null);
    try {
      const res = await api.post(`/videos/${videoId}/transcribe`, { model: selectedModel || null });
      setVideo(res.data);
      setCaptionsVersion((v) => v + 1);
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to transcribe video");
    } finally {
      setIsTranscribing(false);
    }
  };

  const handlePolishCaptions = async () => {
    setIsPolishing(true);
    setError(null);
    try {
      const res = await api.post(`/videos/${videoId}/polish-captions`);
      setVideo(res.data);
      setCaptionsVersion((v) => v + 1);
      setShowReport(true);
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to polish captions");
    } finally {
      setIsPolishing(false);
    }
  };

  const handleRewriteCaptions = async () => {
    setIsRewriting(true);
    setError(null);
    try {
      const res = await api.post(`/videos/${videoId}/rewrite-captions`);
      setVideo(res.data);
      setCaptionsVersion((v) => v + 1);
      setShowReport(true);
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to rewrite captions");
    } finally {
      setIsRewriting(false);
    }
  };

  const handleSrtFileSelected = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (!file) return;

    setIsUploadingSrt(true);
    setError(null);

    const formData = new FormData();
    formData.append("file", file);

    try {
      const res = await api.post(`/videos/${videoId}/captions`, formData, {
        headers: { "Content-Type": "multipart/form-data" },
      });
      setVideo(res.data);
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to upload SRT");
    } finally {
      setIsUploadingSrt(false);
      if (srtInputRef.current) srtInputRef.current.value = "";
    }
  };

  const handleDownloadSrt = async () => {
    try {
      const res = await api.get(`/videos/${videoId}/captions.srt`, { responseType: "blob" });
      const base = video?.original_name?.replace(/\.[^.]+$/, "") || `video_${videoId}`;
      triggerBlobDownload(res.data, `${base}.srt`);
    } catch (err) {
      console.error(err);
      setError("Failed to download SRT");
    }
  };

  const handleExtractFrames = async () => {
    const parsedCount = Number.parseInt(frameCount, 10);
    if (!Number.isInteger(parsedCount) || parsedCount < 1) {
      setError("Please enter a frame count of at least 1.");
      return;
    }

    setIsExtractingFrames(true);
    setError(null);

    try {
      const res = await api.post(`/videos/${videoId}/frames`, { count: parsedCount });
      setFrames(res.data.frames);
      setSelectedFrames(new Set());
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to extract frames");
    } finally {
      setIsExtractingFrames(false);
    }
  };

  const toggleFrameSelection = (filename: string) => {
    setSelectedFrames((prev) => {
      const next = new Set(prev);
      if (next.has(filename)) next.delete(filename);
      else next.add(filename);
      return next;
    });
  };

  const handleDownloadAllFrames = async () => {
    setIsDownloadingAll(true);
    setError(null);
    try {
      const res = await api.get(`/videos/${videoId}/frames/download`, { responseType: "blob" });
      triggerBlobDownload(res.data, `video_${videoId}_frames.zip`);
    } catch (err) {
      console.error(err);
      setError("Failed to download frames");
    } finally {
      setIsDownloadingAll(false);
    }
  };

  const handleDownloadSelectedFrames = async () => {
    if (selectedFrames.size === 0) return;
    setIsDownloadingSelected(true);
    setError(null);
    try {
      const res = await api.post(
        `/videos/${videoId}/frames/download`,
        { filenames: Array.from(selectedFrames) },
        { responseType: "blob" }
      );
      triggerBlobDownload(res.data, `video_${videoId}_frames_selected.zip`);
    } catch (err) {
      console.error(err);
      setError("Failed to download selected frames");
    } finally {
      setIsDownloadingSelected(false);
    }
  };

  const startRenaming = () => {
    setNameDraft(video?.original_name ?? "");
    setIsRenaming(true);
  };

  const handleSaveName = async () => {
    const trimmed = nameDraft.trim();
    if (!trimmed) {
      setError("Name must not be empty.");
      return;
    }
    if (trimmed === video?.original_name) {
      setIsRenaming(false);
      return;
    }

    setIsSavingName(true);
    setError(null);
    try {
      const res = await api.patch(`/videos/${videoId}`, { original_name: trimmed });
      setVideo(res.data);
      setIsRenaming(false);
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to rename video");
    } finally {
      setIsSavingName(false);
    }
  };

  const handleDeleteVideo = async () => {
    if (!video || !confirm(`Delete "${video.original_name}"?`)) return;
    setIsDeleting(true);
    setError(null);
    try {
      await api.delete(`/videos/${videoId}`);
      router.push("/");
    } catch (err) {
      console.error(err);
      setError("Failed to delete video");
      setIsDeleting(false);
    }
  };

  const seekToCue = (start: number) => {
    if (videoRef.current) {
      videoRef.current.currentTime = start;
      videoRef.current.play();
    }
  };

  const startEditingCue = (index: number, text: string) => {
    const el = videoRef.current;
    wasPlayingRef.current = !!el && !el.paused;
    el?.pause();
    setEditingCueIndex(index);
    setCueDraft(text);
  };

  const cancelEditingCue = () => {
    setEditingCueIndex(null);
    setCueDraft("");
    if (wasPlayingRef.current) {
      wasPlayingRef.current = false;
      videoRef.current?.play();
    }
  };

  const closeFindReplace = () => {
    setShowFindReplace(false);
    setFindText("");
    setReplaceText("");
    setReplaceNotice(null);
    setMatchCursor(0);
  };

  const jumpToMatch = (offset: number) => {
    if (!matchedIndexes.length) return;
    const next = (matchCursor + offset + matchedIndexes.length) % matchedIndexes.length;
    setMatchCursor(next);
    cueRefs.current[matchedIndexes[next]]?.scrollIntoView({ behavior: "smooth", block: "center" });
  };

  /** Replace across every line ("all") or in one cue only (its index). */
  const handleReplace = async (scope: "all" | number) => {
    if (!findText) return;
    setReplacingScope(scope);
    setError(null);
    setReplaceNotice(null);
    try {
      const res = await api.post<CaptionReplaceResult>(`/videos/${videoId}/captions/replace`, {
        find: findText,
        replace: replaceText,
        match_case: matchCase,
        segment_indexes: scope === "all" ? null : [scope],
        save_to_glossary: saveToGlossary,
      });
      setVideo(res.data.video);
      setCaptionsVersion((v) => v + 1);
      setMatchCursor(0);
      setReplaceNotice(
        `แทนที่ ${res.data.occurrences} จุด ใน ${res.data.segments_changed} บรรทัด` +
          (res.data.glossary_rule_id ? " · บันทึกเข้าระบบคำแล้ว" : "")
      );
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to replace caption text");
    } finally {
      setReplacingScope(null);
    }
  };

  const handleSaveCue = async (index: number) => {
    const trimmed = cueDraft.trim();
    if (!trimmed) {
      setError("Caption text must not be empty.");
      return;
    }
    if (trimmed === video?.caption_segments?.[index]?.text) {
      cancelEditingCue();
      return;
    }

    setSavingCueIndex(index);
    setError(null);
    try {
      const res = await api.patch(`/videos/${videoId}/captions/${index}`, { text: trimmed });
      setVideo(res.data);
      setCaptionsVersion((v) => v + 1);
      cancelEditingCue();
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to save caption");
    } finally {
      setSavingCueIndex(null);
    }
  };

  useEffect(() => {
    let active = true;

    (async () => {
      try {
        const res = await api.get(`/videos/${videoId}`);
        if (active) setVideo(res.data);
      } catch (err) {
        console.error(err);
        if (active) setError("Failed to load video data.");
      } finally {
        if (active) setLoading(false);
      }
    })();

    (async () => {
      try {
        const res = await api.get(`/videos/${videoId}/frames`);
        if (active && res.data.frames.length > 0) setFrames(res.data.frames);
      } catch (err) {
        console.error(err);
      }
    })();

    (async () => {
      try {
        const res = await api.get<TranscribeModel[]>(`/transcribe-models`);
        if (!active) return;
        setTranscribeModels(res.data);
        const stored = localStorage.getItem(MODEL_KEY);
        const fallback = res.data.find((m) => m.is_default) ?? res.data[0];
        const initial = res.data.find((m) => m.id === stored) ?? fallback;
        setSelectedModel(initial?.id ?? "");
      } catch (err) {
        console.error(err);
      }
    })();

    return () => {
      active = false;
    };
  }, [videoId]);

  // Pre-flight estimate for the chosen model, so the cost is on screen before the
  // click. Only meaningful while the video has no transcript yet.
  useEffect(() => {
    let active = true;
    (async () => {
      if (!selectedModel || video?.has_transcript) {
        if (active) setEstimate(null);
        return;
      }
      try {
        const res = await api.get<TranscribeEstimate>(
          `/videos/${videoId}/transcribe-estimate`,
          { params: { model: selectedModel } }
        );
        if (active) setEstimate(res.data);
      } catch (err) {
        console.error(err);
        if (active) setEstimate(null);
      }
    })();
    return () => {
      active = false;
    };
  }, [videoId, selectedModel, video?.has_transcript]);

  useEffect(() => {
    const el = videoRef.current;
    const segments = video?.caption_segments;
    if (!el || !segments?.length) return;

    const handleTimeUpdate = () => {
      const t = el.currentTime;
      const idx = segments.findIndex((s) => t >= s.start && t < s.end);
      setActiveCueIndex(idx === -1 ? null : idx);
    };

    el.addEventListener("timeupdate", handleTimeUpdate);
    return () => el.removeEventListener("timeupdate", handleTimeUpdate);
  }, [video?.caption_segments]);

  // Ctrl/Cmd+F opens our caption search instead of the browser's, which cannot see
  // past the cue list's scroll container anyway. Esc closes it.
  useEffect(() => {
    if (!video?.caption_segments?.length) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "f") {
        event.preventDefault();
        setShowFindReplace(true);
        // The input may only mount on this render, so focus on the next frame.
        requestAnimationFrame(() => findInputRef.current?.select());
      }
      if (event.key === "Escape" && showFindReplace && editingCueIndex === null) {
        closeFindReplace();
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [video?.caption_segments?.length, showFindReplace, editingCueIndex]);

  useEffect(() => {
    // Following playback would fight the search for control of the scroll position.
    if (activeCueIndex !== null && editingCueIndex === null && !showFindReplace) {
      cueRefs.current[activeCueIndex]?.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }, [activeCueIndex, editingCueIndex, showFindReplace]);

  useEffect(() => {
    if (activeFrameIndex === null) {
      document.body.style.overflow = "";
      return;
    }

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "ArrowLeft") {
        event.preventDefault();
        setActiveFrameIndex((currentIndex) => {
          if (currentIndex === null || frames.length === 0) return currentIndex;
          return (currentIndex - 1 + frames.length) % frames.length;
        });
      }
      if (event.key === "ArrowRight") {
        event.preventDefault();
        setActiveFrameIndex((currentIndex) => {
          if (currentIndex === null || frames.length === 0) return currentIndex;
          return (currentIndex + 1) % frames.length;
        });
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setActiveFrameIndex(null);
      }
    };

    document.body.style.overflow = "hidden";
    window.addEventListener("keydown", handleKeyDown);

    return () => {
      document.body.style.overflow = "";
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [activeFrameIndex, frames.length]);

  const showPreviousFrame = () => {
    setActiveFrameIndex((currentIndex) => {
      if (currentIndex === null || frames.length === 0) return currentIndex;
      return (currentIndex - 1 + frames.length) % frames.length;
    });
  };

  const showNextFrame = () => {
    setActiveFrameIndex((currentIndex) => {
      if (currentIndex === null || frames.length === 0) return currentIndex;
      return (currentIndex + 1) % frames.length;
    });
  };

  const activeFrame = activeFrameIndex === null ? null : frames[activeFrameIndex];

  if (loading) {
    return (
      <div className="flex-1 flex flex-col items-center justify-center gap-3 text-gray-400 py-24">
        <Loader2 className="w-6 h-6 animate-spin" strokeWidth={1.5} />
        <p className="text-sm">Loading video...</p>
      </div>
    );
  }

  if (!video) {
    return (
      <div className="flex-1 flex flex-col items-center justify-center text-gray-400 gap-3">
        <AlertCircle className="w-8 h-8" strokeWidth={1.5} />
        <h2 className="text-lg font-medium text-gray-700">Video not found</h2>
        <Link href="/" className="text-blue-600 hover:text-blue-700 flex items-center gap-1 text-sm">
          <ArrowLeft className="w-4 h-4" strokeWidth={1.5} /> Back to videos
        </Link>
      </div>
    );
  }

  return (
    <main className="flex-1 max-w-6xl w-full mx-auto px-8 py-12 flex flex-col gap-10">
      <div className="flex items-center gap-3">
        <button
          onClick={() => window.history.back()}
          aria-label="Go back"
          title="Go back"
          className="p-2 rounded-full hover:bg-gray-100 transition-colors text-gray-500"
        >
          <ArrowLeft className="w-5 h-5" strokeWidth={1.5} />
        </button>
        {isRenaming ? (
          <div className="flex items-center gap-2 flex-1 min-w-0">
            <input
              autoFocus
              value={nameDraft}
              onChange={(e) => setNameDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleSaveName();
                if (e.key === "Escape") setIsRenaming(false);
              }}
              disabled={isSavingName}
              className="flex-1 min-w-0 text-2xl font-semibold tracking-tight text-gray-900 border-b border-gray-300 focus:border-blue-500 outline-none bg-transparent disabled:opacity-50"
            />
            <button
              onClick={handleSaveName}
              disabled={isSavingName}
              aria-label="Save name"
              title="Save"
              className="p-2 rounded-full hover:bg-gray-100 transition-colors text-blue-600 disabled:opacity-50"
            >
              {isSavingName ? (
                <Loader2 className="w-5 h-5 animate-spin" strokeWidth={1.5} />
              ) : (
                <Check className="w-5 h-5" strokeWidth={1.5} />
              )}
            </button>
            <button
              onClick={() => setIsRenaming(false)}
              disabled={isSavingName}
              aria-label="Cancel rename"
              title="Cancel"
              className="p-2 rounded-full hover:bg-gray-100 transition-colors text-gray-500 disabled:opacity-50"
            >
              <X className="w-5 h-5" strokeWidth={1.5} />
            </button>
          </div>
        ) : (
          <div className="flex items-center gap-2 flex-1 min-w-0">
            <h1 className="text-2xl font-semibold tracking-tight text-gray-900 truncate" title={video.original_name}>
              {video.original_name}
            </h1>
            <button
              onClick={startRenaming}
              aria-label="Rename video"
              title="Rename"
              className="shrink-0 p-2 rounded-full hover:bg-gray-100 transition-colors text-gray-400 hover:text-gray-600"
            >
              <Pencil className="w-4 h-4" strokeWidth={1.5} />
            </button>
            <button
              onClick={handleDeleteVideo}
              disabled={isDeleting}
              aria-label="Delete video"
              title="Delete"
              className="shrink-0 p-2 rounded-full hover:bg-red-50 transition-colors text-gray-400 hover:text-red-600 disabled:opacity-50"
            >
              {isDeleting ? (
                <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
              ) : (
                <Trash2 className="w-4 h-4" strokeWidth={1.5} />
              )}
            </button>
          </div>
        )}
      </div>

      {error && (
        <p className="flex items-center gap-2 text-sm text-red-600">
          <AlertCircle className="w-4 h-4" strokeWidth={1.5} />
          {error}
        </p>
      )}

      {/* Player + captions side by side */}
      <div className="grid lg:grid-cols-[1fr_380px] gap-8 items-start">
        <div className="flex flex-col gap-3">
          <div className="aspect-video bg-black rounded-2xl overflow-hidden">
            <video
              ref={videoRef}
              src={withAuthToken(`${MEDIA_BASE}/api/videos/stream/${video.filename}`)}
              controls
              className="w-full h-full object-contain"
            >
              {video.has_transcript && (
                <track
                  key={captionsVersion}
                  kind="subtitles"
                  src={withAuthToken(`${MEDIA_BASE}/api/videos/${videoId}/captions.vtt?v=${captionsVersion}`)}
                  srcLang="th"
                  label="Thai"
                  default
                />
              )}
            </video>
          </div>
          <div className="flex flex-wrap justify-between items-center gap-3">
            <span className="flex items-center gap-1.5 text-sm text-gray-500">
              <span className={`w-1.5 h-1.5 rounded-full ${video.has_transcript ? "bg-blue-500" : "bg-gray-300"}`} />
              {video.has_transcript ? "Transcribed" : "Not transcribed"}
            </span>
            <div className="flex items-center gap-2">
              <input
                ref={srtInputRef}
                type="file"
                accept=".srt"
                className="hidden"
                onChange={handleSrtFileSelected}
              />
              <button
                onClick={() => srtInputRef.current?.click()}
                disabled={isUploadingSrt}
                title={video.has_transcript ? "Upload an SRT file to replace the current captions" : "Upload an SRT file instead of transcribing"}
                className="flex items-center gap-2 border border-gray-300 text-gray-700 px-4 py-2 rounded-full text-sm font-medium hover:bg-gray-50 disabled:opacity-50 transition-colors"
              >
                {isUploadingSrt ? <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} /> : <Upload className="w-4 h-4" strokeWidth={1.5} />}
                {isUploadingSrt ? "Uploading..." : video.has_transcript ? "Replace SRT" : "Upload SRT"}
              </button>
              {!video.has_transcript && transcribeModels.length > 0 && (
                <select
                  value={selectedModel}
                  onChange={(e) => {
                    setSelectedModel(e.target.value);
                    localStorage.setItem(MODEL_KEY, e.target.value);
                  }}
                  disabled={isTranscribing}
                  aria-label="Transcription model"
                  title={transcribeModels.find((m) => m.id === selectedModel)?.description || "Transcription model"}
                  className="border border-gray-300 text-gray-700 px-4 py-2 rounded-full text-sm bg-white focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400 disabled:opacity-50"
                >
                  {Array.from(new Set(transcribeModels.map((m) => m.provider))).map((provider) => (
                    <optgroup key={provider} label={PROVIDER_LABELS[provider] ?? provider}>
                      {transcribeModels
                        .filter((m) => m.provider === provider)
                        .map((m) => (
                          <option key={m.id} value={m.id}>
                            {m.label}
                            {m.is_default ? " (default)" : ""}
                          </option>
                        ))}
                    </optgroup>
                  ))}
                </select>
              )}
              {!video.has_transcript && (
                <button
                  onClick={handleTranscribe}
                  disabled={isTranscribing}
                  className="bg-blue-600 text-white px-5 py-2 rounded-full text-sm font-medium hover:bg-blue-700 disabled:opacity-50 flex items-center gap-2 transition-colors"
                >
                  {isTranscribing ? <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} /> : <FileText className="w-4 h-4" strokeWidth={1.5} />}
                  {isTranscribing ? "Transcribing..." : "Transcribe"}
                </button>
              )}
            </div>
          </div>

          {/* Projected time and cost before the run */}
          {!video.has_transcript && !isTranscribing && estimate?.model === selectedModel && (
            <p className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-gray-500">
              <span className="flex items-center gap-1">
                <Clock className="w-3.5 h-3.5" strokeWidth={1.5} />
                ประมาณ {formatDuration(estimate.estimated_seconds)}
              </span>
              <span className="flex items-center gap-1">
                <DollarSign className="w-3.5 h-3.5" strokeWidth={1.5} />
                ประมาณ {formatCost(estimate.estimated_cost_usd)}
              </span>
              <span className="text-gray-400">
                เสียง {formatDuration(estimate.audio_seconds)} · {estimate.basis}
                {estimate.is_estimate_only ? " · ราคายังไม่ยืนยัน" : ""}
              </span>
            </p>
          )}

          {/* What the run actually cost */}
          {video.transcribe_stats && (
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-xl bg-gray-50 px-4 py-2.5 text-xs text-gray-600">
              <span className="font-medium text-gray-900">{video.transcribe_stats.model}</span>
              <span className="flex items-center gap-1">
                <Clock className="w-3.5 h-3.5" strokeWidth={1.5} />
                {formatDuration(video.transcribe_stats.elapsed_seconds)}
                {video.transcribe_stats.realtime_factor
                  ? ` (${video.transcribe_stats.realtime_factor}x realtime)`
                  : ""}
              </span>
              <span className="flex items-center gap-1">
                <DollarSign className="w-3.5 h-3.5" strokeWidth={1.5} />
                {formatCost(video.transcribe_stats.estimated_cost_usd)}
              </span>
              {video.transcribe_stats.audio_seconds ? (
                <span className="text-gray-400">
                  เสียง {formatDuration(video.transcribe_stats.audio_seconds)}
                </span>
              ) : null}
              {video.transcribe_stats.requests ? (
                <span className="text-gray-400">{video.transcribe_stats.requests} requests</span>
              ) : null}
              <span className="text-gray-400">· {video.transcribe_stats.cost_basis} (ประมาณการ)</span>
              {video.rewrite_stats && (
                <span className="text-gray-400">
                  · rewrite {video.rewrite_stats.model}{" "}
                  {formatDuration(video.rewrite_stats.elapsed_seconds)}{" "}
                  {formatCost(video.rewrite_stats.estimated_cost_usd)}
                </span>
              )}
            </div>
          )}
        </div>

        {/* Captions panel beside the video */}
        {(video.has_transcript || isTranscribing) && (
          <aside className="flex flex-col gap-3 lg:sticky lg:top-24">
            {video.has_transcript && (
              // No "Captions" label needed here: the panel sits right next to the
              // cue list, so a heading would only repeat what is already obvious.
              <div className="flex flex-wrap items-center gap-1.5">
                <button
                  onClick={handlePolishCaptions}
                  disabled={isPolishing || isRewriting}
                  title="แก้คำผิดตามระบบคำ + ตัดบรรทัดสั้น + จัดเวลา (ไม่มีค่าใช้จ่าย)"
                  className="flex items-center gap-1.5 whitespace-nowrap border border-gray-300 text-gray-700 px-3 py-1.5 rounded-full text-xs font-medium hover:bg-gray-50 disabled:opacity-50 transition-colors"
                >
                  {isPolishing ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={1.5} /> : <Wand2 className="w-3.5 h-3.5" strokeWidth={1.5} />}
                  {isPolishing ? "กำลังขัดคำ..." : "ขัดคำ"}
                </button>
                <button
                  onClick={handleRewriteCaptions}
                  disabled={isRewriting || isPolishing}
                  title="แก้คำผิดและปรับคำบรรยายให้อ่านลื่นขึ้นด้วย AI (มีค่าใช้จ่าย)"
                  className="flex items-center gap-1.5 whitespace-nowrap border border-gray-300 text-gray-700 px-3 py-1.5 rounded-full text-xs font-medium hover:bg-gray-50 disabled:opacity-50 transition-colors"
                >
                  {isRewriting ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={1.5} /> : <Sparkles className="w-3.5 h-3.5" strokeWidth={1.5} />}
                  {isRewriting ? "กำลังแก้ด้วย AI..." : "แก้ด้วย AI"}
                </button>
                <button
                  onClick={() => {
                    if (showFindReplace) closeFindReplace();
                    else {
                      setShowFindReplace(true);
                      requestAnimationFrame(() => findInputRef.current?.focus());
                    }
                  }}
                  title="ค้นหาและแทนที่คำในคำบรรยาย (Ctrl+F)"
                  aria-expanded={showFindReplace}
                  className={`flex items-center gap-1.5 whitespace-nowrap border px-3 py-1.5 rounded-full text-xs font-medium transition-colors ${
                    showFindReplace
                      ? "border-blue-500 bg-blue-50 text-blue-700"
                      : "border-gray-300 text-gray-700 hover:bg-gray-50"
                  }`}
                >
                  <Search className="w-3.5 h-3.5" strokeWidth={1.5} />
                  หาคำ/แทนที่
                </button>
                <button
                  onClick={handleDownloadSrt}
                  title="Download SRT"
                  className="flex items-center gap-1.5 whitespace-nowrap border border-gray-300 text-gray-700 px-3 py-1.5 rounded-full text-xs font-medium hover:bg-gray-50 transition-colors"
                >
                  <Download className="w-3.5 h-3.5" strokeWidth={1.5} />
                  SRT
                </button>
              </div>
            )}

            {isRewriting && (
              <p className="flex items-center gap-2 text-xs text-blue-600">
                <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={1.5} />
                กำลังแก้คำผิดและปรับคำบรรยายด้วย AI...
              </p>
            )}

            {isPolishing && (
              <p className="flex items-center gap-2 text-xs text-blue-600">
                <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={1.5} />
                แก้คำผิด ตัดบรรทัด และจัดเวลาใหม่...
              </p>
            )}

            {/* Find & replace over the caption lines */}
            {showFindReplace && video.has_transcript && (
              <div className="flex flex-col gap-2 rounded-xl border border-blue-100 bg-blue-50/40 px-3 py-3">
                <div className="flex items-center gap-1.5">
                  <Search className="w-3.5 h-3.5 shrink-0 text-gray-400" strokeWidth={1.5} />
                  <input
                    ref={findInputRef}
                    autoFocus
                    value={findText}
                    onChange={(e) => {
                      setFindText(e.target.value);
                      setMatchCursor(0);
                      setReplaceNotice(null);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        e.preventDefault();
                        jumpToMatch(e.shiftKey ? -1 : 1);
                      }
                    }}
                    placeholder="ค้นหาคำ"
                    className="min-w-0 flex-1 rounded-lg border border-gray-200 bg-white px-2.5 py-1.5 text-sm text-gray-900 focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
                  />
                  <button
                    onClick={() => jumpToMatch(-1)}
                    disabled={!matchedIndexes.length}
                    aria-label="บรรทัดก่อนหน้าที่พบ"
                    title="บรรทัดก่อนหน้า (Shift+Enter)"
                    className="shrink-0 rounded-md p-1.5 text-gray-500 hover:bg-white hover:text-gray-800 disabled:opacity-40 transition-colors"
                  >
                    <ChevronUp className="w-3.5 h-3.5" strokeWidth={1.5} />
                  </button>
                  <button
                    onClick={() => jumpToMatch(1)}
                    disabled={!matchedIndexes.length}
                    aria-label="บรรทัดถัดไปที่พบ"
                    title="บรรทัดถัดไป (Enter)"
                    className="shrink-0 rounded-md p-1.5 text-gray-500 hover:bg-white hover:text-gray-800 disabled:opacity-40 transition-colors"
                  >
                    <ChevronDown className="w-3.5 h-3.5" strokeWidth={1.5} />
                  </button>
                  <button
                    onClick={() => setMatchCase((v) => !v)}
                    aria-pressed={matchCase}
                    title="ตรงตามตัวพิมพ์ใหญ่-เล็ก"
                    className={`shrink-0 rounded-md p-1.5 transition-colors ${
                      matchCase
                        ? "bg-blue-600 text-white"
                        : "text-gray-500 hover:bg-white hover:text-gray-800"
                    }`}
                  >
                    <CaseSensitive className="w-3.5 h-3.5" strokeWidth={1.5} />
                  </button>
                  <button
                    onClick={closeFindReplace}
                    aria-label="ปิดการค้นหา"
                    title="ปิด (Esc)"
                    className="shrink-0 rounded-md p-1.5 text-gray-400 hover:bg-white hover:text-gray-700 transition-colors"
                  >
                    <X className="w-3.5 h-3.5" strokeWidth={1.5} />
                  </button>
                </div>

                <div className="flex items-center gap-1.5">
                  <Replace className="w-3.5 h-3.5 shrink-0 text-gray-400" strokeWidth={1.5} />
                  <input
                    value={replaceText}
                    onChange={(e) => {
                      setReplaceText(e.target.value);
                      setReplaceNotice(null);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && findText && totalMatches > 0) {
                        e.preventDefault();
                        handleReplace("all");
                      }
                    }}
                    placeholder="แทนที่ด้วย"
                    className="min-w-0 flex-1 rounded-lg border border-gray-200 bg-white px-2.5 py-1.5 text-sm text-gray-900 focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
                  />
                  <button
                    onClick={() => handleReplace("all")}
                    disabled={!findText || totalMatches === 0 || replacingScope !== null}
                    title="แทนที่ทุกจุดที่พบ"
                    className="flex shrink-0 items-center gap-1.5 rounded-full bg-blue-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-blue-700 disabled:opacity-40 transition-colors"
                  >
                    {replacingScope === "all" ? (
                      <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={1.5} />
                    ) : (
                      <Check className="w-3.5 h-3.5" strokeWidth={1.5} />
                    )}
                    แทนที่ทั้งหมด
                  </button>
                </div>

                <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
                  <span className="text-gray-500">
                    {!findText
                      ? "พิมพ์คำที่ต้องการค้นหา"
                      : totalMatches === 0
                      ? "ไม่พบคำนี้ในคำบรรยาย"
                      : `พบ ${totalMatches} จุด ใน ${matchedIndexes.length} บรรทัด` +
                        (matchedIndexes.length > 1 ? ` · อยู่ที่ ${matchCursor + 1}/${matchedIndexes.length}` : "")}
                  </span>
                  <label className="flex items-center gap-1.5 text-gray-600 cursor-pointer">
                    <input
                      type="checkbox"
                      checked={saveToGlossary}
                      onChange={(e) => setSaveToGlossary(e.target.checked)}
                      className="rounded border-gray-300 text-blue-600 focus:ring-blue-500/30"
                    />
                    บันทึกคู่คำเข้าระบบคำ
                  </label>
                </div>

                {replaceNotice && <p className="text-xs font-medium text-green-700">{replaceNotice}</p>}
                <p className="text-[11px] text-gray-400">
                  แทนที่แล้วบันทึกทับคำบรรยายทันที
                </p>
              </div>
            )}

            {/* The find & replace history panel is hidden from the UI per request —
                every run is still recorded in `caption_replacements` on the backend and
                readable via GET /api/videos/{id}/captions/replacements if needed later. */}

            {/* What the clean-up pass changed */}
            {video.polish_report && (
              <div className="flex flex-col gap-2 rounded-xl border border-gray-100 bg-gray-50/60 px-4 py-3">
                <button
                  onClick={() => setShowReport((v) => !v)}
                  className="flex flex-wrap items-center gap-x-2 gap-y-1 text-left text-xs text-gray-600"
                >
                  <span className="font-medium text-gray-900">
                    แก้คำ {video.polish_report.correction_count} จุด
                  </span>
                  <span className="text-gray-400">
                    · {video.polish_report.segments_in} → {video.polish_report.segments_out} บรรทัด
                    {video.polish_report.line_budget
                      ? ` (${video.polish_report.line_budget.min}-${video.polish_report.line_budget.max} ตัวอักษร)`
                      : ""}
                  </span>
                  {video.polish_report.timing_issues.length > 0 && (
                    <span className="text-gray-400">
                      · จัดเวลา {video.polish_report.timing_issues.length} จุด
                    </span>
                  )}
                  <span className="text-blue-600">{showReport ? "ซ่อน" : "ดูรายละเอียด"}</span>
                </button>

                {showReport && (
                  <div className="flex flex-col gap-3">
                    {video.polish_report.by_rule.length > 0 ? (
                      <div className="flex flex-col gap-1.5">
                        {video.polish_report.by_rule.map((rule) => (
                          <div key={rule.rule_id} className="flex flex-wrap items-center gap-1.5 text-xs">
                            <span
                              title={rule.category_label}
                              className={`rounded-full border px-2 py-0.5 ${categoryClass(rule.category)}`}
                            >
                              {rule.after}
                            </span>
                            <span className="text-gray-400">←</span>
                            {rule.variants.map(([before, count]) => (
                              <span key={before} className="text-gray-500">
                                <s className="opacity-70">{before}</s>
                                {count > 1 ? ` ×${count}` : ""}
                              </span>
                            ))}
                          </div>
                        ))}
                      </div>
                    ) : (
                      <p className="text-xs text-gray-400">ไม่พบคำที่ต้องแก้</p>
                    )}
                    {video.polish_report.timing_issues.length > 0 && (
                      <p className="text-[11px] text-gray-400">
                        แก้ timestamp:{" "}
                        {video.polish_report.timing_issues.filter((i) => i.kind === "overlap").length} ทับซ้อน,{" "}
                        {video.polish_report.timing_issues.filter((i) => i.kind === "zero_duration").length} เวลา 0 วินาที
                      </p>
                    )}
                    <p className="text-[11px] text-gray-400">
                      tokenizer: {video.polish_report.tokenizer} ·{" "}
                      <Link href="/glossary" className="text-blue-600 hover:underline">
                        จัดการระบบคำ
                      </Link>
                    </p>
                  </div>
                )}
              </div>
            )}

            {video.caption_segments?.length ? (
              <div className="flex flex-col max-h-[60vh] overflow-y-auto rounded-xl border border-gray-100">
                {video.caption_segments.map((seg, i) => {
                  const isEditing = editingCueIndex === i;
                  const isSavingCue = savingCueIndex === i;
                  // Line splitting renumbers cues, so match the report's corrections
                  // by the text that is actually on this line.
                  const lineCorrections = (video.polish_report?.by_rule ?? [])
                    .filter((rule) => seg.text.includes(rule.after))
                    .map((rule) => ({
                      before: rule.variants[0]?.[0] ?? "",
                      after: rule.after,
                      category: rule.category,
                      category_label: rule.category_label,
                      rule_id: rule.rule_id,
                    }));
                  const searchHits = showFindReplace ? countMatches(seg.text, findText, matchCase) : 0;
                  const isCurrentMatch = searchHits > 0 && matchedIndexes[matchCursor] === i;
                  return (
                    <div
                      key={i}
                      ref={(el) => {
                        cueRefs.current[i] = el;
                      }}
                      className={`group px-4 py-2.5 text-sm border-b border-gray-50 last:border-b-0 transition-colors ${
                        activeCueIndex === i && !isEditing ? "bg-blue-50 text-blue-900" : "text-gray-700"
                      } ${isEditing ? "bg-white" : "hover:bg-gray-50"} ${
                        isCurrentMatch ? "ring-2 ring-inset ring-amber-300" : ""
                      }`}
                    >
                      {isEditing ? (
                        <div className="flex flex-col gap-2">
                          <span className="text-gray-400 tabular-nums text-xs">{formatTimestamp(seg.start)}</span>
                          <textarea
                            autoFocus
                            rows={2}
                            value={cueDraft}
                            onChange={(e) => setCueDraft(e.target.value)}
                            onKeyDown={(e) => {
                              if (e.key === "Enter" && !e.shiftKey) {
                                e.preventDefault();
                                handleSaveCue(i);
                              }
                              if (e.key === "Escape") cancelEditingCue();
                            }}
                            disabled={isSavingCue}
                            className="w-full resize-y rounded-lg border border-gray-200 px-3 py-2 text-sm text-gray-900 focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400 disabled:opacity-50"
                          />
                          <div className="flex items-center gap-2">
                            <button
                              onClick={() => handleSaveCue(i)}
                              disabled={isSavingCue}
                              className="flex items-center gap-1.5 bg-blue-600 text-white px-3 py-1.5 rounded-full text-xs font-medium hover:bg-blue-700 disabled:opacity-50 transition-colors"
                            >
                              {isSavingCue ? (
                                <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={1.5} />
                              ) : (
                                <Check className="w-3.5 h-3.5" strokeWidth={1.5} />
                              )}
                              {isSavingCue ? "Saving..." : "Save"}
                            </button>
                            <button
                              onClick={cancelEditingCue}
                              disabled={isSavingCue}
                              className="flex items-center gap-1.5 whitespace-nowrap border border-gray-300 text-gray-700 px-3 py-1.5 rounded-full text-xs font-medium hover:bg-gray-50 disabled:opacity-50 transition-colors"
                            >
                              <X className="w-3.5 h-3.5" strokeWidth={1.5} />
                              Cancel
                            </button>
                            <span className="text-[11px] text-gray-400">Enter to save, Esc to cancel</span>
                          </div>
                        </div>
                      ) : (
                        <div className="flex items-start gap-2">
                          <button
                            onClick={() => seekToCue(seg.start)}
                            onDoubleClick={() => startEditingCue(i, seg.text)}
                            title="Click to jump here, double-click to edit"
                            className="flex-1 min-w-0 text-left flex gap-3"
                          >
                            <span className="text-gray-400 tabular-nums shrink-0">{formatTimestamp(seg.start)}</span>
                            <span className="whitespace-pre-wrap">
                              {searchHits > 0
                                ? // While searching, the hits are what matters on this line.
                                  splitOnMatches(seg.text, findText, matchCase).map((part, k) =>
                                    part.isMatch ? (
                                      <mark key={k} className="rounded bg-amber-200 px-0.5 text-gray-900">
                                        {part.text}
                                      </mark>
                                    ) : (
                                      <span key={k}>{part.text}</span>
                                    )
                                  )
                                : highlightSegments(seg.text, lineCorrections).map((part, k) =>
                                    part.category ? (
                                      <mark
                                        key={k}
                                        title={`แก้จาก: ${
                                          lineCorrections.find((c) => c.after === part.text)?.before ?? ""
                                        }`}
                                        className={`rounded px-0.5 border-b ${categoryClass(part.category)}`}
                                      >
                                        {part.text}
                                      </mark>
                                    ) : (
                                      <span key={k}>{part.text}</span>
                                    )
                                  )}
                            </span>
                          </button>
                          {searchHits > 0 && (
                            <button
                              onClick={() => handleReplace(i)}
                              disabled={replacingScope !== null}
                              title={`แทนที่เฉพาะบรรทัดนี้ (${searchHits} จุด)`}
                              className="flex shrink-0 items-center gap-1 rounded-full border border-amber-300 bg-amber-50 px-2 py-1 text-[11px] font-medium text-amber-800 hover:bg-amber-100 disabled:opacity-40 transition-colors"
                            >
                              {replacingScope === i ? (
                                <Loader2 className="w-3 h-3 animate-spin" strokeWidth={1.5} />
                              ) : (
                                <Replace className="w-3 h-3" strokeWidth={1.5} />
                              )}
                              แทนที่
                            </button>
                          )}
                          <button
                            onClick={() => startEditingCue(i, seg.text)}
                            aria-label={`Edit caption at ${formatTimestamp(seg.start)}`}
                            title="Edit text"
                            className="shrink-0 p-1 rounded-md text-gray-400 opacity-0 group-hover:opacity-100 focus:opacity-100 hover:text-gray-700 hover:bg-gray-100 transition-all"
                          >
                            <Pencil className="w-3.5 h-3.5" strokeWidth={1.5} />
                          </button>
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            ) : (
              <p className="flex items-center gap-2 text-sm text-gray-500">
                <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
                Transcription in progress. Please wait...
              </p>
            )}
          </aside>
        )}
      </div>

      {/* Frame extraction */}
      <div className="flex flex-col gap-4 pt-8 border-t border-gray-100">
        <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <h2 className="text-lg font-semibold text-gray-900">Frames</h2>
            <p className="text-sm text-gray-500 mt-1">Extract evenly spaced frames from the video.</p>
          </div>
          <div className="flex items-center gap-3">
            <input
              type="number"
              min={1}
              step={1}
              value={frameCount}
              onChange={(e) => setFrameCount(e.target.value)}
              className="w-20 border border-gray-200 rounded-full px-4 py-2 text-sm text-center focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
            />
            <button
              onClick={handleExtractFrames}
              disabled={isExtractingFrames}
              className="border border-gray-300 text-gray-700 px-5 py-2 rounded-full text-sm font-medium hover:bg-gray-50 disabled:opacity-50 flex items-center gap-2 transition-colors"
            >
              {isExtractingFrames ? <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} /> : <Camera className="w-4 h-4" strokeWidth={1.5} />}
              {isExtractingFrames ? "Extracting..." : "Extract"}
            </button>
          </div>
        </div>

        {isExtractingFrames && (
          <p className="flex items-center gap-2 text-sm text-gray-500">
            <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
            Extracting frames, please wait...
          </p>
        )}

        {frames.length > 0 && (
          <>
            <div className="flex flex-wrap items-center gap-3">
              <button
                onClick={handleDownloadAllFrames}
                disabled={isDownloadingAll}
                className="flex items-center gap-2 bg-blue-600 text-white px-4 py-2 rounded-full text-sm font-medium hover:bg-blue-700 disabled:opacity-50 transition-colors"
              >
                {isDownloadingAll ? <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} /> : <Archive className="w-4 h-4" strokeWidth={1.5} />}
                {isDownloadingAll ? "Preparing..." : `Download all (${frames.length})`}
              </button>
              <button
                onClick={handleDownloadSelectedFrames}
                disabled={selectedFrames.size === 0 || isDownloadingSelected}
                className="flex items-center gap-2 border border-gray-300 text-gray-700 px-4 py-2 rounded-full text-sm font-medium hover:bg-gray-50 disabled:opacity-50 transition-colors"
              >
                {isDownloadingSelected ? <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} /> : <Download className="w-4 h-4" strokeWidth={1.5} />}
                {isDownloadingSelected ? "Preparing..." : `Download selected (${selectedFrames.size})`}
              </button>
              {selectedFrames.size > 0 && (
                <button
                  onClick={() => setSelectedFrames(new Set())}
                  className="text-sm text-gray-500 hover:text-gray-700 transition-colors"
                >
                  Clear selection
                </button>
              )}
            </div>

            <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-3 pt-1">
              {frames.map((frame, index) => {
                const selected = selectedFrames.has(frame.filename);
                return (
                  <div
                    key={frame.filename}
                    className={`relative rounded-xl overflow-hidden bg-gray-50 aspect-video group ring-2 transition-all ${
                      selected ? "ring-blue-500" : "ring-transparent"
                    }`}
                  >
                    <button
                      type="button"
                      onClick={() => setActiveFrameIndex(index)}
                      className="w-full h-full"
                      aria-label={`Open extracted frame ${index + 1}`}
                    >
                      <img
                        src={withAuthToken(`${MEDIA_BASE}${frame.url}`)}
                        alt={`Extracted frame ${index + 1}`}
                        className="w-full h-full object-cover transition-transform duration-200 group-hover:scale-[1.02]"
                      />
                    </button>
                    <button
                      type="button"
                      onClick={() => toggleFrameSelection(frame.filename)}
                      aria-label={selected ? "Deselect frame" : "Select frame"}
                      className={`absolute top-2 left-2 p-1 rounded-md backdrop-blur transition-colors ${
                        selected ? "bg-blue-600 text-white" : "bg-black/40 text-white hover:bg-black/60"
                      }`}
                    >
                      {selected ? <CheckSquare className="w-4 h-4" strokeWidth={1.5} /> : <Square className="w-4 h-4" strokeWidth={1.5} />}
                    </button>
                  </div>
                );
              })}
            </div>
          </>
        )}
      </div>

      {activeFrame && (
        <div
          className="fixed inset-0 z-50 bg-black/90 p-4 sm:p-8 flex items-center justify-center"
          role="dialog"
          aria-modal="true"
          aria-label={`Extracted frame ${activeFrameIndex! + 1}`}
          onClick={() => setActiveFrameIndex(null)}
        >
          <button
            type="button"
            onClick={() => setActiveFrameIndex(null)}
            className="absolute top-4 right-4 sm:top-6 sm:right-6 p-2 rounded-full bg-white/10 text-white hover:bg-white/20 transition-colors"
            aria-label="Close image viewer"
          >
            <X className="w-5 h-5" strokeWidth={1.5} />
          </button>

          {frames.length > 1 && (
            <button
              type="button"
              onClick={(event) => {
                event.stopPropagation();
                showPreviousFrame();
              }}
              className="absolute left-3 sm:left-6 p-3 rounded-full bg-white/10 text-white hover:bg-white/20 transition-colors"
              aria-label="View previous image"
            >
              <ChevronLeft className="w-6 h-6" strokeWidth={1.5} />
            </button>
          )}

          <div
            className="relative w-full max-w-6xl max-h-full flex flex-col gap-4 items-center"
            onClick={(event) => event.stopPropagation()}
          >
            <img
              src={withAuthToken(`${MEDIA_BASE}${activeFrame.url}`)}
              alt={`Extracted frame ${activeFrameIndex! + 1}`}
              className="max-w-full max-h-[75vh] object-contain rounded-2xl"
            />

            <div className="w-full max-w-4xl rounded-2xl bg-white/10 backdrop-blur px-4 py-3 text-white flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
              <div>
                <p className="text-sm font-medium">Frame {activeFrameIndex! + 1} of {frames.length}</p>
                <p className="text-sm text-white/60 break-all">{activeFrame.filename}</p>
              </div>
              <a
                href={withAuthToken(`${MEDIA_BASE}${activeFrame.url}`)}
                download={activeFrame.filename}
                className="inline-flex items-center justify-center gap-2 rounded-full border border-white/20 text-white px-4 py-2 text-sm font-medium hover:bg-white/10 transition-colors"
              >
                <Download className="w-4 h-4" strokeWidth={1.5} />
                Download
              </a>
            </div>
          </div>

          {frames.length > 1 && (
            <button
              type="button"
              onClick={(event) => {
                event.stopPropagation();
                showNextFrame();
              }}
              className="absolute right-3 sm:right-6 p-3 rounded-full bg-white/10 text-white hover:bg-white/20 transition-colors"
              aria-label="View next image"
            >
              <ChevronRight className="w-6 h-6" strokeWidth={1.5} />
            </button>
          )}
        </div>
      )}
    </main>
  );
}
