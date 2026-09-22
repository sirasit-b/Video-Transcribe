"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import axios from "axios";
import { Upload, Loader2, AlertCircle, Film, Trash2, Search, Plus, GripVertical, X, Check, CircleX } from "lucide-react";
import { api } from "./lib/api";

interface VideoItem {
  id: number;
  filename: string;
  original_name: string;
  has_transcript: boolean;
  project_id: number | null;
  created_at: string;
}

interface ProjectItem {
  id: number;
  name: string;
  video_count: number;
}

type Tab = "all" | "ungrouped" | number;

interface UploadTask {
  id: string;
  name: string;
  progress: number;
  status: "pending" | "uploading" | "done" | "error";
  error?: string;
}

// How many files upload at once. High enough that a batch actually saturates the
// connection instead of trickling in one at a time, low enough not to make the
// browser (or the server) juggle dozens of simultaneous multi-GB streams.
const MAX_CONCURRENT_UPLOADS = 4;

// A dropped file often arrives with no type at all — the browser only knows the
// common ones, and a .mkv or a .MP4 straight off a camera can come through blank.
// So the extension gets a say too, rather than turning away a real video.
const VIDEO_EXTENSIONS = [
  "mp4", "mov", "m4v", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "mts", "m2ts", "ts", "3gp",
];

function isVideoFile(file: File): boolean {
  if (file.type.startsWith("video/")) return true;
  if (file.type) return false;
  const extension = file.name.split(".").pop()?.toLowerCase() ?? "";
  return VIDEO_EXTENSIONS.includes(extension);
}

/** Whether a drag is carrying files, as opposed to the page's own row reordering
 *  or a selection of text from somewhere else. */
function dragHasFiles(event: React.DragEvent | DragEvent): boolean {
  const types = event.dataTransfer?.types;
  return types ? Array.from(types).includes("Files") : false;
}

export default function Home() {
  const [videos, setVideos] = useState<VideoItem[]>([]);
  const [projects, setProjects] = useState<ProjectItem[]>([]);
  const [activeTab, setActiveTab] = useState<Tab>("all");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [uploadTasks, setUploadTasks] = useState<UploadTask[]>([]);
  const [deletingId, setDeletingId] = useState<number | null>(null);
  const [movingId, setMovingId] = useState<number | null>(null);
  const [search, setSearch] = useState("");
  const [isCreatingProject, setIsCreatingProject] = useState(false);
  const [newProjectName, setNewProjectName] = useState("");
  const [savingProject, setSavingProject] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dragIdRef = useRef<number | null>(null);
  const [isDraggingFiles, setIsDraggingFiles] = useState(false);
  // Dragging over a child fires `dragleave` on its parent, so the overlay cannot
  // be driven by the events alone; counting enter against leave can.
  const dragDepthRef = useRef(0);
  // One queue, drained by a fixed pool. Held in refs rather than state because the
  // pool reads them as it goes, and a render must not be what decides what uploads
  // next.
  const queueRef = useRef<{ task: UploadTask; file: File; projectId: Tab }[]>([]);
  const workersRef = useRef(0);
  // What is on screen, for uploads that finish after the tab has moved on.
  const activeTabRef = useRef<Tab>("all");

  const loadProjects = async () => {
    try {
      const res = await api.get<ProjectItem[]>("/projects");
      setProjects(res.data);
    } catch (err) {
      console.error(err);
    }
  };

  const loadVideos = async (tab: Tab) => {
    setLoading(true);
    setError(null);
    try {
      const params =
        tab === "all" ? {} : tab === "ungrouped" ? { ungrouped: true } : { project_id: tab };
      const res = await api.get<VideoItem[]>("/videos", { params });
      setVideos(res.data);
    } catch (err) {
      console.error(err);
      setError("Could not load videos. Is the backend running?");
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    loadProjects();
  }, []);

  useEffect(() => {
    activeTabRef.current = activeTab;
    loadVideos(activeTab);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTab]);

  /** Catch dragged files anywhere on the page.
   *
   *  On the window rather than on a drop zone, because the default behaviour for a
   *  file dropped anywhere else is for the browser to open it — navigating away
   *  from the page and abandoning whatever was still uploading. Every drop is
   *  caught; one over the page uploads, one that misses is simply ignored. */
  useEffect(() => {
    const onDragEnter = (event: DragEvent) => {
      if (!dragHasFiles(event)) return;
      dragDepthRef.current += 1;
      setIsDraggingFiles(true);
    };
    const onDragOver = (event: DragEvent) => {
      if (!dragHasFiles(event)) return;
      // Without this the drop never fires and the browser opens the file instead.
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
      uploadFiles(Array.from(event.dataTransfer?.files ?? []), activeTabRef.current);
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** One file's upload: the request body is the raw file, not multipart — the server
   *  streams it straight to disk, which is what makes this fast, and it lets a plain
   *  byte-progress callback drive the bar below instead of an approximate multipart one. */
  const uploadOneFile = async (task: UploadTask, file: File, projectId: Tab) => {
    setUploadTasks((prev) => prev.map((t) => (t.id === task.id ? { ...t, status: "uploading" } : t)));
    try {
      const params: Record<string, string | number> = { filename: file.name };
      if (typeof projectId === "number") params.project_id = projectId;

      const res = await api.post<VideoItem>("/videos", file, {
        params,
        headers: { "Content-Type": file.type || "application/octet-stream" },
        onUploadProgress: (evt) => {
          const progress = evt.total ? Math.round((evt.loaded / evt.total) * 100) : 0;
          setUploadTasks((prev) => prev.map((t) => (t.id === task.id ? { ...t, progress } : t)));
        },
      });
      // Uploads now outlive the tab they were started from, so a file only joins
      // the list on screen when that is where it actually went.
      const showing = activeTabRef.current;
      const belongsHere =
        showing === "all" ||
        (showing === "ungrouped" && res.data.project_id === null) ||
        showing === res.data.project_id;
      if (belongsHere) setVideos((prev) => [res.data, ...prev]);
      if (typeof projectId === "number") loadProjects();
      setUploadTasks((prev) => prev.map((t) => (t.id === task.id ? { ...t, status: "done", progress: 100 } : t)));
      // Nothing left pointing at it after this — clear the row so a batch of uploads
      // doesn't leave a permanent checklist behind once everything has succeeded.
      setTimeout(() => {
        setUploadTasks((prev) => prev.filter((t) => t.id !== task.id));
      }, 2000);
    } catch (err) {
      console.error(err);
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setUploadTasks((prev) =>
        prev.map((t) => (t.id === task.id ? { ...t, status: "error", error: detail || "Upload failed" } : t))
      );
    }
  };

  /** Start as many workers as the pool still allows; each drains the shared queue
   *  until it is empty.
   *
   *  One queue for the page rather than one per batch, so files added while a batch
   *  is running join the line instead of starting a second pool beside it — three
   *  drops in a row would otherwise have twelve uploads fighting over the
   *  connection, and each one would crawl. */
  const pumpQueue = () => {
    while (workersRef.current < MAX_CONCURRENT_UPLOADS && queueRef.current.length > 0) {
      workersRef.current += 1;
      void (async () => {
        try {
          for (;;) {
            const item = queueRef.current.shift();
            if (!item) break;
            await uploadOneFile(item.task, item.file, item.projectId);
          }
        } finally {
          workersRef.current -= 1;
        }
      })();
    }
  };

  /** Queue files for upload. Safe to call while others are still going. */
  const uploadFiles = (files: File[], projectId: Tab) => {
    const accepted = files.filter(isVideoFile);
    const rejected = files.length - accepted.length;
    setError(
      rejected > 0
        ? `${rejected} file${rejected > 1 ? "s were" : " was"} not a video and ${
            rejected > 1 ? "were" : "was"
          } skipped.`
        : null
    );
    if (accepted.length === 0) return;

    const tasks: UploadTask[] = accepted.map((file) => ({
      id: `${Date.now()}-${Math.random().toString(36).slice(2)}`,
      name: file.name,
      progress: 0,
      status: "pending",
    }));
    setUploadTasks((prev) => [...prev, ...tasks]);
    queueRef.current.push(
      ...tasks.map((task, index) => ({ task, file: accepted[index], projectId }))
    );
    pumpQueue();
  };

  const dismissUploadTask = (id: string) => {
    setUploadTasks((prev) => prev.filter((t) => t.id !== id));
  };

  const handleFileSelected = (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = Array.from(e.target.files ?? []);
    e.target.value = "";
    uploadFiles(files, activeTab);
  };

  const handleDelete = async (e: React.MouseEvent, video: VideoItem) => {
    e.preventDefault();
    e.stopPropagation();
    if (!confirm(`Delete "${video.original_name}"?`)) return;
    setDeletingId(video.id);
    try {
      await api.delete(`/videos/${video.id}`);
      setVideos((prev) => prev.filter((v) => v.id !== video.id));
      if (video.project_id !== null) loadProjects();
    } catch (err) {
      console.error(err);
      setError("Failed to delete video.");
    } finally {
      setDeletingId(null);
    }
  };

  const handleMove = async (video: VideoItem, targetProjectId: number | null) => {
    if (targetProjectId === video.project_id) return;
    setMovingId(video.id);
    setError(null);
    try {
      const res = await api.patch<VideoItem>(`/videos/${video.id}/move`, { project_id: targetProjectId });
      const stillVisible =
        activeTab === "all" ||
        (activeTab === "ungrouped" ? targetProjectId === null : targetProjectId === activeTab);
      setVideos((prev) =>
        stillVisible ? prev.map((v) => (v.id === video.id ? res.data : v)) : prev.filter((v) => v.id !== video.id)
      );
      loadProjects();
    } catch (err) {
      console.error(err);
      setError("Failed to move video.");
    } finally {
      setMovingId(null);
    }
  };

  const handleCreateProject = async () => {
    const name = newProjectName.trim();
    if (!name) {
      setIsCreatingProject(false);
      return;
    }
    setSavingProject(true);
    try {
      const res = await api.post<ProjectItem>("/projects", { name });
      setProjects((prev) => [...prev, res.data]);
      setActiveTab(res.data.id);
      setNewProjectName("");
      setIsCreatingProject(false);
    } catch (err) {
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to create project.");
    } finally {
      setSavingProject(false);
    }
  };

  const canReorder = activeTab !== "all" && search.trim() === "";

  const handleDrop = async (targetId: number) => {
    const draggedId = dragIdRef.current;
    dragIdRef.current = null;
    if (!canReorder || draggedId === null || draggedId === targetId) return;

    const current = [...videos];
    const fromIndex = current.findIndex((v) => v.id === draggedId);
    const toIndex = current.findIndex((v) => v.id === targetId);
    if (fromIndex === -1 || toIndex === -1) return;

    const [moved] = current.splice(fromIndex, 1);
    current.splice(toIndex, 0, moved);
    setVideos(current);

    try {
      await api.post("/videos/reorder", {
        project_id: activeTab === "ungrouped" ? null : activeTab,
        video_ids: current.map((v) => v.id),
      });
    } catch (err) {
      console.error(err);
      setError("Failed to save the new order.");
      loadVideos(activeTab);
    }
  };

  const filteredVideos = videos.filter((v) =>
    v.original_name.toLowerCase().includes(search.trim().toLowerCase())
  );

  const tabButtonClass = (active: boolean) =>
    `shrink-0 px-3.5 py-1.5 rounded-full text-sm font-medium transition-colors ${
      active ? "bg-gray-900 text-white" : "bg-gray-100 text-gray-600 hover:bg-gray-200"
    }`;

  const activeUploads = uploadTasks.filter((t) => t.status === "pending" || t.status === "uploading");
  const isUploading = activeUploads.length > 0;

  return (
    <main className="flex-1 max-w-3xl w-full mx-auto px-8 py-16 flex flex-col gap-8">
      {/* Shown while files are over the window, so it is obvious the page will
          take them — and where. Pointer events off: it is a sign, not a target,
          and the drop is caught on the window either way. */}
      {isDraggingFiles && (
        <div className="fixed inset-4 z-50 pointer-events-none rounded-3xl border-2 border-dashed border-blue-400 bg-blue-50/80 backdrop-blur-[2px] flex flex-col items-center justify-center gap-3">
          <Upload className="w-10 h-10 text-blue-600" strokeWidth={1.5} />
          <p className="text-lg font-medium text-blue-900">Drop to upload</p>
          <p className="text-sm text-blue-700">
            {typeof activeTab === "number"
              ? `Into ${projects.find((p) => p.id === activeTab)?.name ?? "this project"}`
              : "Videos only — anything else is skipped"}
          </p>
        </div>
      )}
      <div className="flex items-start justify-between gap-6 rounded-2xl">
        <div>
          <h1 className="text-4xl font-semibold tracking-tight text-gray-900">Videos</h1>
          <p className="text-gray-500 mt-2">
            Upload videos to transcribe and extract frames — pick several at once, or drop them here.
          </p>
        </div>
        {/* Never disabled: the moment someone most wants to add another file is
            while the last one is still going, and the queue is built to take it. */}
        <button
          onClick={() => fileInputRef.current?.click()}
          title={isUploading ? "Add more — they join the queue" : undefined}
          className="shrink-0 bg-blue-600 text-white px-5 py-2.5 rounded-full font-medium hover:bg-blue-700 flex items-center gap-2 transition-colors"
        >
          {isUploading ? (
            <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
          ) : (
            <Upload className="w-4 h-4" strokeWidth={1.5} />
          )}
          {isUploading ? `Uploading ${activeUploads.length} — add more` : "Upload Videos"}
        </button>
        <input
          ref={fileInputRef}
          type="file"
          accept="video/*"
          multiple
          className="hidden"
          onChange={handleFileSelected}
        />
      </div>

      {uploadTasks.length > 0 && (
        <div className="flex flex-col gap-1.5 rounded-xl border border-gray-100 bg-gray-50/60 px-4 py-3">
          {uploadTasks.map((task) => (
            <div key={task.id} className="flex items-center gap-3 text-sm">
              <span className="flex-1 min-w-0 truncate text-gray-700" title={task.name}>
                {task.name}
              </span>
              {task.status === "error" ? (
                <span className="shrink-0 text-xs text-red-600">{task.error || "Upload failed"}</span>
              ) : task.status === "done" ? (
                <Check className="w-4 h-4 shrink-0 text-green-600" strokeWidth={1.5} />
              ) : (
                <div className="w-28 shrink-0 h-1.5 rounded-full bg-gray-200 overflow-hidden">
                  <div
                    className="h-full bg-blue-600 transition-all"
                    style={{ width: `${task.progress}%` }}
                  />
                </div>
              )}
              <button
                onClick={() => dismissUploadTask(task.id)}
                aria-label={`Dismiss ${task.name}`}
                className="shrink-0 p-0.5 rounded-full text-gray-400 hover:text-gray-700 hover:bg-gray-200 transition-colors"
              >
                <CircleX className="w-3.5 h-3.5" strokeWidth={1.5} />
              </button>
            </div>
          ))}
        </div>
      )}

      <div className="flex items-center gap-2 flex-wrap">
        <button onClick={() => setActiveTab("all")} className={tabButtonClass(activeTab === "all")}>
          All
        </button>
        <button onClick={() => setActiveTab("ungrouped")} className={tabButtonClass(activeTab === "ungrouped")}>
          Ungrouped
        </button>
        {projects.map((p) => (
          <button
            key={p.id}
            onClick={() => setActiveTab(p.id)}
            className={tabButtonClass(activeTab === p.id)}
          >
            {p.name}
            <span className="ml-1.5 opacity-60">{p.video_count}</span>
          </button>
        ))}
        {isCreatingProject ? (
          <div className="flex items-center gap-1">
            <input
              autoFocus
              value={newProjectName}
              onChange={(e) => setNewProjectName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleCreateProject();
                if (e.key === "Escape") {
                  setIsCreatingProject(false);
                  setNewProjectName("");
                }
              }}
              placeholder="Project name"
              disabled={savingProject}
              className="border border-gray-200 rounded-full px-3.5 py-1.5 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400 disabled:opacity-50"
            />
            <button
              onClick={handleCreateProject}
              disabled={savingProject}
              aria-label="Save project"
              className="p-1.5 rounded-full text-blue-600 hover:bg-blue-50 disabled:opacity-50"
            >
              {savingProject ? <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} /> : <Check className="w-4 h-4" strokeWidth={1.5} />}
            </button>
            <button
              onClick={() => {
                setIsCreatingProject(false);
                setNewProjectName("");
              }}
              disabled={savingProject}
              aria-label="Cancel"
              className="p-1.5 rounded-full text-gray-400 hover:bg-gray-100 disabled:opacity-50"
            >
              <X className="w-4 h-4" strokeWidth={1.5} />
            </button>
          </div>
        ) : (
          <button
            onClick={() => setIsCreatingProject(true)}
            className="shrink-0 flex items-center gap-1 px-3.5 py-1.5 rounded-full text-sm font-medium text-gray-500 border border-dashed border-gray-300 hover:bg-gray-50 transition-colors"
          >
            <Plus className="w-3.5 h-3.5" strokeWidth={1.5} />
            New project
          </button>
        )}
      </div>

      {error && (
        <p className="flex items-center gap-2 text-sm text-red-600">
          <AlertCircle className="w-4 h-4" strokeWidth={1.5} />
          {error}
        </p>
      )}

      {videos.length > 0 && (
        <div className="relative">
          <Search className="w-4 h-4 text-gray-400 absolute left-4 top-1/2 -translate-y-1/2" strokeWidth={1.5} />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search videos by name..."
            className="w-full border border-gray-200 rounded-xl pl-10 pr-4 py-2.5 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
          />
        </div>
      )}

      {loading ? (
        <div className="flex-1 flex items-center justify-center py-24">
          <Loader2 className="w-6 h-6 animate-spin text-gray-400" strokeWidth={1.5} />
        </div>
      ) : videos.length === 0 ? (
        <div className="text-center py-24 text-gray-400">
          <Film className="w-8 h-8 mx-auto mb-3" strokeWidth={1.5} />
          <p className="text-sm">No videos yet. Upload one to get started.</p>
        </div>
      ) : filteredVideos.length === 0 ? (
        <div className="text-center py-24 text-gray-400">
          <Search className="w-8 h-8 mx-auto mb-3" strokeWidth={1.5} />
          <p className="text-sm">No videos match &ldquo;{search}&rdquo;.</p>
        </div>
      ) : (
        <div className="flex flex-col">
          {filteredVideos.map((video) => (
            <Link
              key={video.id}
              href={`/videos/${video.id}`}
              draggable={canReorder}
              onDragStart={() => {
                dragIdRef.current = video.id;
              }}
              // A row dropped somewhere that is not another row leaves the drag
              // id behind; cleared here so the next drop cannot act on it.
              onDragEnd={() => {
                dragIdRef.current = null;
              }}
              onDragOver={(e) => {
                if (canReorder && !dragHasFiles(e)) e.preventDefault();
              }}
              onDrop={(e) => {
                // Files land on rows like anywhere else on the page, and are the
                // window's to handle — a row must not read one as a reorder.
                if (dragHasFiles(e)) return;
                e.preventDefault();
                handleDrop(video.id);
              }}
              className="flex items-center justify-between gap-4 py-4 border-b border-gray-100 hover:bg-gray-50 transition-colors -mx-4 px-4 rounded-lg"
            >
              <span className="flex items-center gap-2 min-w-0">
                {canReorder && (
                  <GripVertical className="w-4 h-4 text-gray-300 shrink-0 cursor-grab" strokeWidth={1.5} />
                )}
                <span className="font-medium text-gray-900 truncate" title={video.original_name}>
                  {video.original_name}
                </span>
              </span>
              <span className="shrink-0 flex items-center gap-3 text-sm text-gray-500">
                <span className="flex items-center gap-1.5">
                  <span
                    className={`w-1.5 h-1.5 rounded-full ${
                      video.has_transcript ? "bg-blue-500" : "bg-gray-300"
                    }`}
                  />
                  {video.has_transcript ? "Transcribed" : "Not transcribed"}
                </span>
                <span onClick={(e) => e.stopPropagation()} className="flex items-center">
                  {movingId === video.id ? (
                    <Loader2 className="w-4 h-4 animate-spin text-gray-400" strokeWidth={1.5} />
                  ) : (
                    <select
                      value={video.project_id === null ? "ungrouped" : String(video.project_id)}
                      onChange={(e) =>
                        handleMove(video, e.target.value === "ungrouped" ? null : Number(e.target.value))
                      }
                      aria-label={`Move ${video.original_name} to a different project`}
                      className="border border-gray-200 rounded-full px-2.5 py-1 text-xs text-gray-500 focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400 bg-white"
                    >
                      <option value="ungrouped">Ungrouped</option>
                      {projects.map((p) => (
                        <option key={p.id} value={p.id}>
                          {p.name}
                        </option>
                      ))}
                    </select>
                  )}
                </span>
                <button
                  onClick={(e) => handleDelete(e, video)}
                  disabled={deletingId === video.id}
                  aria-label={`Delete ${video.original_name}`}
                  className="p-1.5 rounded-full text-gray-400 hover:text-red-600 hover:bg-red-50 disabled:opacity-50 transition-colors"
                >
                  {deletingId === video.id ? (
                    <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
                  ) : (
                    <Trash2 className="w-4 h-4" strokeWidth={1.5} />
                  )}
                </button>
              </span>
            </Link>
          ))}
        </div>
      )}
    </main>
  );
}
