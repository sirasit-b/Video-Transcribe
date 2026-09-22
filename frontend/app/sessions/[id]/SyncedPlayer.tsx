"use client";

import { useEffect, useRef, useState } from "react";
import { Pause, Play, Volume2, VolumeX, SkipBack } from "lucide-react";
import { MEDIA_BASE, TOKEN_KEY } from "../../lib/api";

export interface Cue {
  start: number;
  end: number;
  text: string;
}

export interface PlayerTrack {
  id: number;
  name: string;
  /** Length of this recording's cut, in seconds. */
  duration: number;
  /** Loudness per bucket over the cut, 0-255. */
  waveform: number[];
  role: string;
}

function withAuthToken(url: string): string {
  const token = typeof window !== "undefined" ? localStorage.getItem(TOKEN_KEY) : null;
  if (!token) return url;
  return `${url}${url.includes("?") ? "&" : "?"}token=${encodeURIComponent(token)}`;
}

function formatTime(seconds: number): string {
  if (!Number.isFinite(seconds)) return "0:00";
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  return `${m}:${s.toString().padStart(2, "0")}`;
}

/** Close enough. Well under a frame, and far enough above the browser's own
 *  jitter that nothing is corrected that does not need to be. */
const IN_STEP_SECONDS = 0.06;
/** Past this, the recording is not drifting — it is somewhere else — and is put
 *  back by seeking, which costs a visible stutter but is over at once. */
const SEEK_SECONDS = 0.4;
/** How often the followers are checked against the leader. A timer rather than
 *  `requestAnimationFrame`, which stops entirely when the window is hidden or
 *  minimised: playback carries on regardless, so the correction has to as well,
 *  or the recordings come back a second apart. Measured at nearly one second
 *  after four seconds of playing with the window in the background. */
const CHECK_INTERVAL_MS = 40;

/** One waveform, as a filled shape around its own centre line. */
function WaveformRow({
  values,
  className,
}: {
  values: number[];
  className: string;
}) {
  if (values.length === 0) return null;
  const width = 1000;
  const height = 100;
  const step = width / values.length;
  // Down one side and back along the other, so the line reads as a body of sound
  // rather than a fence of bars.
  const top = values
    .map((v, i) => `${(i * step).toFixed(2)},${(height / 2 - (v / 255) * (height / 2)).toFixed(2)}`)
    .join(" ");
  const bottom = values
    .map(
      (v, i) =>
        `${((values.length - 1 - i) * step).toFixed(2)},${(
          height / 2 +
          (values[values.length - 1 - i] / 255) * (height / 2)
        ).toFixed(2)}`
    )
    .join(" ");

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      className={`w-full h-12 ${className}`}
    >
      <polygon points={`${top} ${bottom}`} fill="currentColor" />
    </svg>
  );
}

/**
 * Every cut of one moment, played as one.
 *
 * The recordings were cut on a single timeline, so the same instant is at the same
 * time in all of them and playing them together is simply playing them all. What
 * that does not survive on its own is the browser: each `<video>` keeps its own
 * clock and they wander apart over minutes, so one of them leads and the rest are
 * pulled back whenever they slip.
 *
 * Only one is audible at a time. Two microphones that heard the same room, played
 * together, comb-filter into something that sounds like neither.
 */
/** The transcript, read along with the playback.
 *
 *  The cues arrive already timed against the cut — the transcript is made from the
 *  recording as it was shot, and the trim takes the silences out of it, so by the
 *  end of a long take the two clocks are minutes apart.
 */
function Transcript({
  cues,
  time,
  onSeek,
}: {
  cues: Cue[];
  time: number;
  onSeek: (seconds: number) => void;
}) {
  const active = cues.findIndex((cue) => time >= cue.start && time < cue.end);
  const containerRef = useRef<HTMLDivElement>(null);
  const activeRef = useRef<HTMLButtonElement>(null);

  // Keep the lit line in the middle, and only move when it changes: scrolling on
  // every frame would fight anyone reading ahead.
  useEffect(() => {
    const line = activeRef.current;
    const box = containerRef.current;
    if (!line || !box) return;
    const wanted = line.offsetTop - box.clientHeight / 2 + line.clientHeight / 2;
    box.scrollTo({ top: Math.max(0, wanted), behavior: "smooth" });
  }, [active]);

  if (cues.length === 0) return null;

  return (
    <div
      ref={containerRef}
      className="max-h-56 overflow-y-auto rounded-xl border border-gray-200 bg-white p-3 flex flex-col gap-0.5"
    >
      {cues.map((cue, index) => (
        <button
          key={`${cue.start}-${index}`}
          ref={index === active ? activeRef : undefined}
          onClick={() => onSeek(cue.start)}
          className={`text-left rounded-lg px-2 py-1 transition-colors ${
            index === active
              ? "bg-blue-50 text-gray-900"
              : index < active
              ? "text-gray-400 hover:bg-gray-50"
              : "text-gray-600 hover:bg-gray-50"
          }`}
        >
          <span className="text-[11px] tabular-nums text-gray-400 mr-2">
            {formatTime(cue.start)}
          </span>
          {cue.text}
        </button>
      ))}
    </div>
  );
}

export default function SyncedPlayer({
  tracks,
  cues = [],
  onAudibleChange,
}: {
  tracks: PlayerTrack[];
  /** The transcript of whichever recording is being listened to. */
  cues?: Cue[];
  onAudibleChange?: (index: number) => void;
}) {
  const videoRefs = useRef<(HTMLVideoElement | null)[]>([]);
  const [playing, setPlaying] = useState(false);
  const [time, setTime] = useState(0);
  const [audible, setAudible] = useState(0);

  // The shortest, so the playhead never runs past the end of one of them.
  const duration = tracks.reduce(
    (shortest, track) => Math.min(shortest, track.duration || Infinity),
    Infinity
  );
  const total = Number.isFinite(duration) ? duration : 0;

  const each = (fn: (video: HTMLVideoElement, index: number) => void) => {
    videoRefs.current.forEach((video, index) => {
      if (video) fn(video, index);
    });
  };

  const togglePlay = () => {
    if (playing) {
      each((video) => video.pause());
      setPlaying(false);
      return;
    }
    // From one position, and started together rather than one by one: each
    // `play()` resolves at its own moment, and four seconds in that was already
    // most of a second of difference.
    const from = videoRefs.current[0]?.currentTime ?? time;
    each((video) => {
      video.currentTime = from;
      video.playbackRate = 1;
      void video.play().catch(() => undefined);
    });
    setPlaying(true);
  };

  const seek = (seconds: number) => {
    const target = Math.max(0, Math.min(total, seconds));
    each((video) => {
      video.currentTime = target;
    });
    setTime(target);
  };

  // The first recording carries the clock; everyone else is held to it.
  //
  // Nudged rather than seeked: a follower that is a little ahead is played very
  // slightly slower until it falls back into step, which is inaudible and
  // invisible, where seeking it every time would stutter several times a second.
  // Seeking is kept for a recording that is properly lost.
  useEffect(() => {
    const timer = setInterval(() => {
      const master = videoRefs.current[0];
      if (!master) return;
      setTime(master.currentTime);
      videoRefs.current.forEach((video, index) => {
        if (!video || index === 0) return;
        const drift = video.currentTime - master.currentTime;
        if (Math.abs(drift) > SEEK_SECONDS) {
          video.currentTime = master.currentTime;
          video.playbackRate = 1;
        } else if (Math.abs(drift) > IN_STEP_SECONDS) {
          // Up to 5% either way, which closes a tenth of a second in a couple of
          // seconds and cannot be heard on a track nobody is listening to.
          video.playbackRate = 1 - Math.max(-0.05, Math.min(0.05, drift * 0.5));
        } else if (video.playbackRate !== 1) {
          video.playbackRate = 1;
        }
      });
    }, CHECK_INTERVAL_MS);
    return () => clearInterval(timer);
  }, []);

  return (
    <div className="flex flex-col gap-4">
      <div
        className={`grid gap-3 ${
          tracks.length > 2 ? "sm:grid-cols-2 lg:grid-cols-3" : "sm:grid-cols-2"
        }`}
      >
        {tracks.map((track, index) => (
          <div key={track.id} className="flex flex-col gap-1.5">
            <div className="relative rounded-xl overflow-hidden bg-black aspect-video">
              <video
                ref={(element) => {
                  videoRefs.current[index] = element;
                }}
                src={withAuthToken(`${MEDIA_BASE}/api/videos/${track.id}/trimmed`)}
                muted={index !== audible}
                playsInline
                preload="auto"
                className="w-full h-full object-contain"
                onEnded={() => setPlaying(false)}
              />
              <button
                onClick={() => {
                  setAudible(index);
                  onAudibleChange?.(index);
                }}
                title={index === audible ? "กำลังฟังเสียงนี้" : "ฟังเสียงของคลิปนี้"}
                className={`absolute bottom-2 right-2 p-1.5 rounded-full transition-colors ${
                  index === audible
                    ? "bg-blue-600 text-white"
                    : "bg-black/50 text-white/70 hover:bg-black/70"
                }`}
              >
                {index === audible ? (
                  <Volume2 className="w-3.5 h-3.5" strokeWidth={1.5} />
                ) : (
                  <VolumeX className="w-3.5 h-3.5" strokeWidth={1.5} />
                )}
              </button>
            </div>
            <p className="text-xs text-gray-600 truncate" title={track.name}>
              {track.name}
              {track.role === "reference" && (
                <span className="text-gray-400"> · ตัวอ้างอิงเวลา</span>
              )}
            </p>
          </div>
        ))}
      </div>

      {/* The lines, stacked rather than overlaid: laid on top of each other they
          would merge into one shape and say nothing about whether they agree. */}
      <div
        className="flex gap-3 rounded-xl border border-gray-200 bg-gray-50/60 p-3 cursor-pointer"
        onClick={(e) => {
          const lanes = e.currentTarget.querySelector("[data-lanes]");
          if (!lanes) return;
          const box = lanes.getBoundingClientRect();
          seek(((e.clientX - box.left) / box.width) * total);
        }}
      >
        <div className="w-28 shrink-0 flex flex-col gap-1">
          {tracks.map((track) => (
            <span
              key={track.id}
              className="h-12 flex items-center text-[11px] text-gray-500 truncate"
              title={track.name}
            >
              {track.name}
            </span>
          ))}
        </div>
        {/* One column holding every line, so a single playhead lands on the same
            instant in all of them — which is the whole point of stacking them. */}
        <div data-lanes className="relative flex-1 min-w-0 flex flex-col gap-1">
          {tracks.map((track, index) => (
            <WaveformRow
              key={track.id}
              values={track.waveform}
              className={index === audible ? "text-blue-600" : "text-gray-400"}
            />
          ))}
          <div
            className="absolute inset-y-0 w-px bg-red-500 pointer-events-none"
            style={{ left: `${total > 0 ? (time / total) * 100 : 0}%` }}
          />
        </div>
      </div>

      <div className="flex items-center gap-3">
        <button
          onClick={togglePlay}
          className="shrink-0 bg-blue-600 text-white w-10 h-10 rounded-full hover:bg-blue-700 flex items-center justify-center transition-colors"
          aria-label={playing ? "หยุด" : "เล่นทุกคลิปพร้อมกัน"}
        >
          {playing ? (
            <Pause className="w-4 h-4" strokeWidth={1.5} />
          ) : (
            <Play className="w-4 h-4 ml-0.5" strokeWidth={1.5} />
          )}
        </button>
        <button
          onClick={() => seek(0)}
          className="shrink-0 text-gray-500 hover:text-gray-700 p-2"
          aria-label="กลับไปต้นคลิป"
        >
          <SkipBack className="w-4 h-4" strokeWidth={1.5} />
        </button>
        <input
          type="range"
          min={0}
          max={Math.max(total, 0.1)}
          step={0.01}
          value={time}
          onChange={(e) => seek(Number(e.target.value))}
          aria-label="ตำแหน่งเวลา"
          className="flex-1 accent-blue-600"
        />
        <span className="shrink-0 text-xs text-gray-500 tabular-nums">
          {formatTime(time)} / {formatTime(total)}
        </span>
      </div>

      <Transcript cues={cues} time={time} onSeek={seek} />
    </div>
  );
}
