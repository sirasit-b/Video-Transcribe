"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import axios from "axios";
import { Upload, Loader2, AlertCircle, Film, Trash2, Search, Plus, GripVertical, X, Check } from "lucide-react";
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

export default function Home() {
  const [videos, setVideos] = useState<VideoItem[]>([]);
  const [projects, setProjects] = useState<ProjectItem[]>([]);
  const [activeTab, setActiveTab] = useState<Tab>("all");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [uploading, setUploading] = useState(false);
  const [deletingId, setDeletingId] = useState<number | null>(null);
  const [movingId, setMovingId] = useState<number | null>(null);
  const [search, setSearch] = useState("");
  const [isCreatingProject, setIsCreatingProject] = useState(false);
  const [newProjectName, setNewProjectName] = useState("");
  const [savingProject, setSavingProject] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dragIdRef = useRef<number | null>(null);

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
    loadVideos(activeTab);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTab]);

  const handleFileSelected = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (!file) return;

    setUploading(true);
    setError(null);

    const formData = new FormData();
    formData.append("file", file);
    if (typeof activeTab === "number") {
      formData.append("project_id", String(activeTab));
    }

    try {
      const res = await api.post<VideoItem>("/videos", formData, {
        headers: { "Content-Type": "multipart/form-data" },
      });
      setVideos((prev) => [res.data, ...prev]);
      if (typeof activeTab === "number") loadProjects();
    } catch (err) {
      console.error(err);
      setError("Failed to upload video.");
    } finally {
      setUploading(false);
      if (fileInputRef.current) fileInputRef.current.value = "";
    }
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

  return (
    <main className="flex-1 max-w-3xl w-full mx-auto px-8 py-16 flex flex-col gap-8">
      <div className="flex items-start justify-between gap-6">
        <div>
          <h1 className="text-4xl font-semibold tracking-tight text-gray-900">Videos</h1>
          <p className="text-gray-500 mt-2">Upload a video to transcribe and extract frames.</p>
        </div>
        <button
          onClick={() => fileInputRef.current?.click()}
          disabled={uploading}
          className="shrink-0 bg-blue-600 text-white px-5 py-2.5 rounded-full font-medium hover:bg-blue-700 disabled:opacity-50 flex items-center gap-2 transition-colors"
        >
          {uploading ? (
            <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
          ) : (
            <Upload className="w-4 h-4" strokeWidth={1.5} />
          )}
          {uploading ? "Uploading..." : "Upload Video"}
        </button>
        <input
          ref={fileInputRef}
          type="file"
          accept="video/*"
          className="hidden"
          onChange={handleFileSelected}
        />
      </div>

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
              onDragOver={(e) => {
                if (canReorder) e.preventDefault();
              }}
              onDrop={(e) => {
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
