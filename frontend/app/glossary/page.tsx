"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import axios from "axios";
import {
  AlertCircle,
  BookMarked,
  Check,
  Loader2,
  Plus,
  Search,
  Shield,
  Trash2,
  Wand2,
} from "lucide-react";
import { api } from "../lib/api";

interface GlossaryRule {
  id: string;
  right: string;
  wrong: string[];
  category: string;
  category_label: string;
  note: string;
  source: string;
  enabled: boolean;
}

interface GlossaryData {
  rules: GlossaryRule[];
  protected: string[];
  categories: Record<string, string>;
  stats: { rule_count: number; variant_count: number; protected_count: number };
}

interface Correction {
  before: string;
  after: string;
  category: string;
  category_label: string;
  rule_id: string;
}

// Matches the colours used for correction highlights on the video page.
const CATEGORY_STYLES: Record<string, string> = {
  symbol: "bg-rose-50 text-rose-700 border-rose-200",
  ui: "bg-orange-50 text-orange-700 border-orange-200",
  filename: "bg-amber-50 text-amber-700 border-amber-200",
  path: "bg-lime-50 text-lime-700 border-lime-200",
  platform: "bg-sky-50 text-sky-700 border-sky-200",
  term: "bg-violet-50 text-violet-700 border-violet-200",
  english: "bg-blue-50 text-blue-700 border-blue-200",
  protected: "bg-gray-100 text-gray-600 border-gray-200",
};

function categoryClass(category: string): string {
  return CATEGORY_STYLES[category] ?? "bg-gray-50 text-gray-600 border-gray-200";
}

export default function GlossaryPage() {
  const [data, setData] = useState<GlossaryData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [categoryFilter, setCategoryFilter] = useState("");
  const [busyRuleId, setBusyRuleId] = useState<string | null>(null);

  const [right, setRight] = useState("");
  const [wrong, setWrong] = useState("");
  const [category, setCategory] = useState("term");
  const [note, setNote] = useState("");
  const [isProtected, setIsProtected] = useState(false);
  const [isSaving, setIsSaving] = useState(false);

  const [sample, setSample] = useState("");
  const [preview, setPreview] = useState<{ after: string; corrections: Correction[] } | null>(null);
  const [isPreviewing, setIsPreviewing] = useState(false);

  const load = useCallback(async () => {
    try {
      const res = await api.get<GlossaryData>("/glossary");
      setData(res.data);
    } catch (err) {
      console.error(err);
      setError("Failed to load the glossary");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    (async () => {
      await load();
    })();
  }, [load]);

  const handleAdd = async (e: React.FormEvent) => {
    e.preventDefault();
    setIsSaving(true);
    setError(null);
    try {
      await api.post("/glossary", {
        right: right.trim(),
        // One misheard spelling per line keeps long variant lists readable.
        wrong: wrong
          .split("\n")
          .map((item) => item.trim())
          .filter(Boolean),
        category: isProtected ? "protected" : category,
        note: note.trim(),
        is_protected: isProtected,
      });
      setRight("");
      setWrong("");
      setNote("");
      setIsProtected(false);
      await load();
    } catch (err) {
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      setError(detail || "Failed to save the rule");
    } finally {
      setIsSaving(false);
    }
  };

  const handleToggle = async (rule: GlossaryRule) => {
    setBusyRuleId(rule.id);
    try {
      await api.patch(`/glossary/${encodeURIComponent(rule.id)}`, { enabled: !rule.enabled });
      await load();
    } catch (err) {
      console.error(err);
      setError("Failed to update the rule");
    } finally {
      setBusyRuleId(null);
    }
  };

  const handleDelete = async (rule: GlossaryRule) => {
    if (!confirm(`Remove the rule for "${rule.right}"?`)) return;
    setBusyRuleId(rule.id);
    try {
      await api.delete(`/glossary/${encodeURIComponent(rule.id)}`);
      await load();
    } catch (err) {
      const detail = axios.isAxiosError(err) ? err.response?.data?.detail : undefined;
      // Built-ins have no row to delete; disabling is the way to switch one off.
      setError(detail || "Only rules you added can be removed. Disable built-ins instead.");
    } finally {
      setBusyRuleId(null);
    }
  };

  const handlePreview = async () => {
    if (!sample.trim()) return;
    setIsPreviewing(true);
    try {
      const res = await api.post("/glossary/preview", { text: sample });
      setPreview({ after: res.data.after, corrections: res.data.corrections });
    } catch (err) {
      console.error(err);
      setError("Preview failed");
    } finally {
      setIsPreviewing(false);
    }
  };

  const visibleRules = useMemo(() => {
    if (!data) return [];
    const needle = query.trim().toLowerCase();
    return data.rules.filter((rule) => {
      if (categoryFilter && rule.category !== categoryFilter) return false;
      if (!needle) return true;
      return (
        rule.right.toLowerCase().includes(needle) ||
        rule.wrong.some((item) => item.toLowerCase().includes(needle))
      );
    });
  }, [data, query, categoryFilter]);

  if (loading) {
    return (
      <main className="max-w-5xl mx-auto px-8 py-16 flex justify-center">
        <Loader2 className="w-6 h-6 animate-spin text-gray-400" strokeWidth={1.5} />
      </main>
    );
  }

  return (
    <main className="max-w-5xl mx-auto px-8 py-10 flex flex-col gap-8">
      <header className="flex flex-col gap-2">
        <h1 className="flex items-center gap-2 text-2xl font-semibold tracking-tight text-gray-900">
          <BookMarked className="w-6 h-6" strokeWidth={1.5} />
          ระบบคำ (Word system)
        </h1>
        <p className="text-sm text-gray-500 max-w-2xl">
          คำที่ ASR ถอดผิดจะถูกแก้ทั้งหมดในรอบเดียว ตามกฎด้านล่าง — คำที่ตั้งใจคงเป็นคำทับศัพท์ไทย
          (โมเดล, เทรน, เซฟ, พาธ ...) จะถูกกันไว้ไม่ให้แปลง
        </p>
        {data && (
          <p className="text-xs text-gray-400 tabular-nums">
            {data.stats.rule_count} rules · {data.stats.variant_count} spellings matched ·{" "}
            {data.stats.protected_count} protected Thai words
          </p>
        )}
      </header>

      {error && (
        <div className="flex items-start gap-2 rounded-xl border border-red-100 bg-red-50 px-4 py-3 text-sm text-red-700">
          <AlertCircle className="w-4 h-4 mt-0.5 shrink-0" strokeWidth={1.5} />
          {error}
        </div>
      )}

      {/* Try the word system on a sample line */}
      <section className="flex flex-col gap-3 rounded-2xl border border-gray-100 p-5">
        <h2 className="text-sm font-semibold text-gray-900">ทดลองแก้คำ</h2>
        <div className="flex flex-col gap-2 sm:flex-row">
          <input
            value={sample}
            onChange={(e) => setSample(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handlePreview();
            }}
            placeholder="เช่น เทรนโมเดล โยโล ด้วยดาต้าเซ็ตจาก Robo4Universe แล้วเซฟเป็น base.pt"
            className="flex-1 rounded-full border border-gray-200 px-4 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
          />
          <button
            onClick={handlePreview}
            disabled={isPreviewing || !sample.trim()}
            className="flex items-center justify-center gap-2 rounded-full bg-gray-900 px-5 py-2 text-sm font-medium text-white hover:bg-gray-800 disabled:opacity-50 transition-colors"
          >
            {isPreviewing ? (
              <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
            ) : (
              <Wand2 className="w-4 h-4" strokeWidth={1.5} />
            )}
            ตรวจ
          </button>
        </div>
        {preview && (
          <div className="flex flex-col gap-2 rounded-xl bg-gray-50 px-4 py-3">
            <p className="text-sm text-gray-900">{preview.after}</p>
            {preview.corrections.length ? (
              <div className="flex flex-wrap gap-1.5">
                {preview.corrections.map((c, i) => (
                  <span
                    key={`${c.rule_id}-${i}`}
                    title={c.category_label}
                    className={`rounded-full border px-2 py-0.5 text-xs ${categoryClass(c.category)}`}
                  >
                    <s className="opacity-60">{c.before}</s> → {c.after}
                  </span>
                ))}
              </div>
            ) : (
              <p className="text-xs text-gray-400">ไม่พบคำที่ต้องแก้</p>
            )}
          </div>
        )}
      </section>

      {/* Add a rule */}
      <section className="rounded-2xl border border-gray-100 p-5">
        <h2 className="mb-3 text-sm font-semibold text-gray-900">เพิ่มคำใหม่</h2>
        <form onSubmit={handleAdd} className="grid gap-3 sm:grid-cols-2">
          <label className="flex flex-col gap-1">
            <span className="text-xs text-gray-500">คำที่ถูกต้อง</span>
            <input
              required
              value={right}
              onChange={(e) => setRight(e.target.value)}
              placeholder="best.pt"
              className="rounded-lg border border-gray-200 px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
            />
          </label>
          <label className="flex flex-col gap-1">
            <span className="text-xs text-gray-500">หมวด</span>
            <select
              value={isProtected ? "protected" : category}
              onChange={(e) => setCategory(e.target.value)}
              disabled={isProtected}
              className="rounded-lg border border-gray-200 px-3 py-2 text-sm bg-white focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400 disabled:opacity-50"
            >
              {data &&
                Object.entries(data.categories)
                  .filter(([key]) => key !== "protected")
                  .map(([key, label]) => (
                    <option key={key} value={key}>
                      {label}
                    </option>
                  ))}
            </select>
          </label>
          <label className="flex flex-col gap-1 sm:col-span-2">
            <span className="text-xs text-gray-500">
              คำที่ถอดผิด (บรรทัดละคำ){isProtected ? " — ไม่ต้องกรอกสำหรับคำที่กันไว้" : ""}
            </span>
            <textarea
              rows={3}
              value={wrong}
              onChange={(e) => setWrong(e.target.value)}
              disabled={isProtected}
              placeholder={"base.pt\nbast.pt\nเบส.pt"}
              className="rounded-lg border border-gray-200 px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400 disabled:opacity-50"
            />
          </label>
          <label className="flex flex-col gap-1 sm:col-span-2">
            <span className="text-xs text-gray-500">หมายเหตุ (ไม่บังคับ)</span>
            <input
              value={note}
              onChange={(e) => setNote(e.target.value)}
              className="rounded-lg border border-gray-200 px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
            />
          </label>
          <label className="flex items-center gap-2 text-sm text-gray-600 sm:col-span-2">
            <input
              type="checkbox"
              checked={isProtected}
              onChange={(e) => setIsProtected(e.target.checked)}
              className="rounded border-gray-300"
            />
            <Shield className="w-3.5 h-3.5 text-gray-400" strokeWidth={1.5} />
            คงเป็นคำทับศัพท์ไทย ไม่ให้กฎใดแปลงคำนี้
          </label>
          <div className="sm:col-span-2">
            <button
              type="submit"
              disabled={isSaving || !right.trim()}
              className="flex items-center gap-2 rounded-full bg-blue-600 px-5 py-2 text-sm font-medium text-white hover:bg-blue-700 disabled:opacity-50 transition-colors"
            >
              {isSaving ? (
                <Loader2 className="w-4 h-4 animate-spin" strokeWidth={1.5} />
              ) : (
                <Plus className="w-4 h-4" strokeWidth={1.5} />
              )}
              บันทึกเข้าระบบคำ
            </button>
          </div>
        </form>
      </section>

      {/* Rule list */}
      <section className="flex flex-col gap-3">
        <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
          <h2 className="text-sm font-semibold text-gray-900">
            กฎทั้งหมด ({visibleRules.length})
          </h2>
          <div className="flex items-center gap-2">
            <div className="relative">
              <Search
                className="pointer-events-none absolute left-3 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-gray-400"
                strokeWidth={1.5}
              />
              <input
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="ค้นหาคำ"
                className="rounded-full border border-gray-200 py-1.5 pl-9 pr-4 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-400"
              />
            </div>
            <select
              value={categoryFilter}
              onChange={(e) => setCategoryFilter(e.target.value)}
              className="rounded-full border border-gray-200 px-3 py-1.5 text-sm bg-white focus:outline-none focus:ring-2 focus:ring-blue-500/30"
            >
              <option value="">ทุกหมวด</option>
              {data &&
                Object.entries(data.categories).map(([key, label]) => (
                  <option key={key} value={key}>
                    {label}
                  </option>
                ))}
            </select>
          </div>
        </div>

        <div className="divide-y divide-gray-50 rounded-2xl border border-gray-100">
          {visibleRules.map((rule) => (
            <div
              key={rule.id}
              className={`flex flex-wrap items-start gap-3 px-4 py-3 ${
                rule.enabled ? "" : "opacity-50"
              }`}
            >
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium text-gray-900">{rule.right}</span>
                  <span
                    className={`rounded-full border px-2 py-0.5 text-[11px] ${categoryClass(rule.category)}`}
                  >
                    {rule.category_label}
                  </span>
                  {rule.source === "user" && (
                    <span className="rounded-full bg-blue-50 px-2 py-0.5 text-[11px] text-blue-600">
                      ของคุณ
                    </span>
                  )}
                </div>
                {rule.wrong.length > 0 && (
                  <p className="mt-1 break-words text-xs text-gray-500">
                    ← {rule.wrong.join(" · ")}
                  </p>
                )}
                {rule.note && <p className="mt-1 text-xs text-gray-400">{rule.note}</p>}
              </div>
              <div className="flex shrink-0 items-center gap-1">
                <button
                  onClick={() => handleToggle(rule)}
                  disabled={busyRuleId === rule.id}
                  title={rule.enabled ? "ปิดกฎนี้" : "เปิดกฎนี้"}
                  className="rounded-md p-1.5 text-gray-400 hover:bg-gray-100 hover:text-gray-700 disabled:opacity-50 transition-colors"
                >
                  {busyRuleId === rule.id ? (
                    <Loader2 className="h-4 w-4 animate-spin" strokeWidth={1.5} />
                  ) : (
                    <Check className={`h-4 w-4 ${rule.enabled ? "text-green-600" : ""}`} strokeWidth={1.5} />
                  )}
                </button>
                {rule.source === "user" && (
                  <button
                    onClick={() => handleDelete(rule)}
                    disabled={busyRuleId === rule.id}
                    title="ลบกฎนี้"
                    className="rounded-md p-1.5 text-gray-400 hover:bg-red-50 hover:text-red-600 disabled:opacity-50 transition-colors"
                  >
                    <Trash2 className="h-4 w-4" strokeWidth={1.5} />
                  </button>
                )}
              </div>
            </div>
          ))}
          {!visibleRules.length && (
            <p className="px-4 py-6 text-center text-sm text-gray-400">ไม่พบกฎที่ตรงกับเงื่อนไข</p>
          )}
        </div>
      </section>

      {/* Protected words */}
      {data && data.protected.length > 0 && (
        <section className="flex flex-col gap-2 rounded-2xl border border-gray-100 p-5">
          <h2 className="flex items-center gap-1.5 text-sm font-semibold text-gray-900">
            <Shield className="w-4 h-4 text-gray-400" strokeWidth={1.5} />
            คำทับศัพท์ไทยที่กันไว้ ({data.protected.length})
          </h2>
          <p className="text-xs text-gray-500">
            คำเหล่านี้จะไม่ถูกแปลงเป็นภาษาอังกฤษ และยังกันกฎอื่นไม่ให้แก้คำที่มีคำนี้อยู่ข้างใน
          </p>
          <div className="flex flex-wrap gap-1.5">
            {data.protected.map((term) => (
              <span
                key={term}
                className="rounded-full border border-gray-200 bg-gray-50 px-2.5 py-0.5 text-xs text-gray-600"
              >
                {term}
              </span>
            ))}
          </div>
        </section>
      )}
    </main>
  );
}
