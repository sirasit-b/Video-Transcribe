"""The word system: a persistent glossary that repairs Thai ASR transcripts.

Everything here is deterministic — no LLM call. A rule maps one canonical spelling
(``right``) to the list of ways the ASR mangles it (``wrong``). All rules and all
protected terms are compiled into a *single* alternation regex, so a transcript is
repaired in exactly one left-to-right pass (`Glossary.correct`). That single pass is
what makes the result predictable:

* every occurrence of every variant is fixed, not just the first one;
* a replacement is never itself re-examined, so rules cannot cascade into each other
  (``base.pt`` -> ``best.pt`` can never be re-read as another rule's input);
* longest variant wins, because the alternation is sorted by length descending;
* a protected term consumes its own text, so no rule can fire *inside* a word we
  deliberately keep as Thai (``โมเดล``, ``เทรน``, ...).

Every replacement is reported back as a correction record, which is what the UI
highlights.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field

# Categories exist so the UI can colour-code highlights and so a reviewer can scan
# "what kind of mistake was this" without reading every rule.
CATEGORY_LABELS: dict[str, str] = {
	"symbol": "เครื่องหมาย / คำสั่ง",
	"ui": "ปุ่ม / UI",
	"filename": "ชื่อไฟล์",
	"path": "พาธ / โฟลเดอร์",
	"platform": "แพลตฟอร์ม / เครื่องมือ",
	"term": "ศัพท์เฉพาะทาง",
	"english": "ศัพท์เทคนิคภาษาอังกฤษ",
	"protected": "คงคำทับศัพท์ไทย",
}

# Thai combining vowels and tone marks. A Thai variant must not match when the next
# character is one of these: it would mean we cut into the middle of a syllable
# (matching "เกต" inside "เกตุ", for instance).
_THAI_COMBINING = "ัิีึืฺุู็่้๊๋์ํ๎"

THAI_CHAR_CLASS = "฀-๿"


def _is_ascii(value: str) -> bool:
	return all(ord(char) < 128 for char in value)


# ---------------------------------------------------------------------------
# Built-in rules
# ---------------------------------------------------------------------------
# `right` is the spelling we want; `wrong` are the transcription errors seen in the
# wild. A canonical form that is ASCII also picks up its own lower-cased form as a
# variant automatically (see `_expand_rule`), which is how casing gets normalised —
# "machine learning" and "MACHINE LEARNING" both land on "Machine Learning".

DEFAULT_RULES: list[dict] = [
	# --- Symbols and spoken punctuation -----------------------------------
	{
		"right": "!",
		"wrong": ["เครื่องหมายตกใจ", "เครื่องหมายอัศเจรีย์", "ตกใจ", "ตะกุยตกใจ"],
		"category": "symbol",
		"note": "ASR ถอดเครื่องหมาย ! ออกมาเป็นคำว่า 'ตกใจ'",
	},
	{
		"right": "?",
		"wrong": ["เครื่องหมายคำถาม", "เครื่องหมายปรัศนี"],
		"category": "symbol",
	},
	{
		"right": "#",
		"wrong": ["เครื่องหมายชาร์ป", "เครื่องหมายแฮช"],
		"category": "symbol",
	},
	# --- Player / UI buttons ----------------------------------------------
	{
		"right": "Play",
		"wrong": ["เพย์", "เพลย์", "เพล", "เป้", "เพ้"],
		"category": "ui",
		"note": "ปุ่ม Play",
	},
	{
		"right": "Pause",
		"wrong": ["พอส", "พอซ", "พ้อส", "ป๊อส", "Post", "โพส", "โพสต์", "พอสต์"],
		"category": "ui",
		"note": "ปุ่ม Pause — ระวัง 'Post' อาจเป็นคำที่ตั้งใจพูดจริงในบางบริบท",
	},
	{
		"right": "Run",
		"wrong": ["กดรัน", "กดรั่น"],
		"category": "ui",
		"note": "ปุ่ม Run ใน Notebook",
	},
	# --- Technical file names ---------------------------------------------
	{
		"right": "best.pt",
		"wrong": [
			"base.pt", "bast.pt", "beast.pt", "bess.pt", "bes.pt", "best.pd",
			"best.pth", "เบส.pt", "เบสท์.pt", "เบส dot pt", "best dot pt",
		],
		"category": "filename",
		"note": "น้ำหนักโมเดลที่ดีที่สุดจากการเทรน — มักถูกฟังผิดเป็น base.pt",
	},
	{
		"right": "last.pt",
		"wrong": ["last.pth", "lash.pt", "แลส.pt", "last dot pt"],
		"category": "filename",
	},
	{
		"right": "data.yaml",
		"wrong": [
			"data.yml", "data.yalm", "data.yaml file", "เดต้า.yaml", "ดาต้า.yaml",
			"data dot yaml", "data dot yml",
		],
		"category": "filename",
		"note": "ไฟล์ config ของ dataset — นามสกุลต้องเป็น .yaml ไม่ใช่ .yml",
	},
	{
		"right": "requirements.txt",
		"wrong": ["requirement.txt", "requirements.text"],
		"category": "filename",
	},
	# --- Paths -------------------------------------------------------------
	# Canonicalised to the real lower-case Colab paths, because these strings get
	# pasted into code — a capitalised "/Content/Run" would simply not resolve.
	{
		"right": "/content/runs/detect/predict",
		"wrong": [
			"/content/run/detect/predict", "content/run/detect/predict",
			"/content/runs/detect/predic", "/content/run/detect/predic",
			"content/runs/detect/predict", "/content/rund/detect/predict",
		],
		"category": "path",
	},
	{
		"right": "/content/runs/detect/train",
		"wrong": [
			"/content/run/detect/train", "content/run/detect/train",
			"content/runs/detect/train", "/content/rund/detect/train",
		],
		"category": "path",
	},
	{
		"right": "runs/detect/predict",
		"wrong": ["run/detect/predict", "runs/detect/predic", "rund/detect/predict"],
		"category": "path",
	},
	{
		"right": "runs/detect/train",
		"wrong": ["run/detect/train", "rund/detect/train"],
		"category": "path",
	},
	{
		"right": "runs/detect",
		"wrong": ["run/detect", "rund/detect", "runs/detec"],
		"category": "path",
	},
	{
		"right": "/content/drive/MyDrive",
		"wrong": [
			"/content/drive/mydrive", "content/drive/mydrive",
			"/content/drive/my drive", "/content/drive/MyDrive/",
		],
		"category": "path",
	},
	{
		"right": "/content/datasets",
		"wrong": ["/content/dataset", "content/datasets", "content/dataset"],
		"category": "path",
	},
	{
		"right": "/content",
		"wrong": ["/contents", "/conten", "/contect"],
		"category": "path",
	},
	# --- Platforms and tools ----------------------------------------------
	# Two-word form first so "Roboflow Universe" is not shortened to "Roboflow"
	# by the single-word rule (length ordering handles this, but keep it readable).
	{
		"right": "Roboflow Universe",
		"wrong": [
			"Robo4Universe", "Robo 4 Universe", "Robo four Universe",
			"RoboFlow Universe", "Robo flow Universe", "Roboflow universe",
			"Roboflow Univers", "Roboflow Universal", "Roboflow ยูนิเวิร์ส",
			"โรโบโฟลว์ ยูนิเวิร์ส", "โรโบโฟล ยูนิเวิร์ส",
		],
		"category": "platform",
	},
	{
		"right": "Roboflow",
		"wrong": [
			"RoboFlow", "Robo flow", "Robo Flow", "Robo-flow", "Roboflo",
			"โรโบโฟลว์", "โรโบโฟล", "โรโบ้โฟลว์", "โรโบโฟว",
		],
		"category": "platform",
	},
	{
		"right": "Google Colab",
		"wrong": [
			"Google colab", "google collab", "Google Collab", "กูเกิลโคแลบ",
			"กูเกิล โคแล็บ", "โคแล็บ", "โคแลบ", "คอลแลป", "Collab",
		],
		"category": "platform",
	},
	{
		"right": "Ultralytics",
		"wrong": [
			"Ultra Analytics", "Ultra lytics", "Ultralitics", "Ultralystic",
			"อัลตราไลติกส์", "อัลตร้าไลติก", "อัลตร้าลิติกส์",
		],
		"category": "platform",
	},
	{
		"right": "YOLO",
		"wrong": ["โยโล", "โยโล่", "Yollo", "You only look once"],
		"category": "platform",
	},
	{"right": "YOLOv8", "wrong": ["yolo v8", "YOLO v8", "โยโลวี8"], "category": "platform"},
	{"right": "YOLOv11", "wrong": ["yolo v11", "YOLO v11", "โยโลวี11"], "category": "platform"},
	{"right": "Kaggle", "wrong": ["แคกเกิล", "แคเกิล", "Kagle"], "category": "platform"},
	{"right": "Hugging Face", "wrong": ["ฮักกิงเฟซ", "Huggingface", "Hugging face"], "category": "platform"},
	{"right": "GitHub", "wrong": ["Github", "git hub", "กิตฮับ", "กิตฮับ"], "category": "platform"},
	{"right": "Jupyter", "wrong": ["จูปิเตอร์", "Jupiter", "จูไพเตอร์"], "category": "platform"},
	{"right": "TensorFlow", "wrong": ["Tensorflow", "Tensor flow", "เทนเซอร์โฟลว์"], "category": "platform"},
	{"right": "PyTorch", "wrong": ["Pytorch", "Py torch", "ไพทอร์ช"], "category": "platform"},
	{"right": "NumPy", "wrong": ["Numpy", "นัมไพ", "นัมปี้"], "category": "platform"},
	{"right": "OpenCV", "wrong": ["Opencv", "open CV", "โอเพนซีวี"], "category": "platform"},
	# --- Logic gates -------------------------------------------------------
	# The compound forms come before bare "เกต" so "แอนด์เกต" becomes "AND Gate"
	# rather than "แอนด์Gate".
	{"right": "AND Gate", "wrong": ["แอนด์เกต", "แอนเกต", "แอนด์เกจ", "แอนด์เกด", "and gate"], "category": "term"},
	{"right": "OR Gate", "wrong": ["ออร์เกต", "ออเกต", "ออร์เกจ", "ออร์เกด", "or gate"], "category": "term"},
	{
		"right": "XOR Gate",
		"wrong": ["เอ็กซ์ออร์เกต", "ซอร์เกต", "เอ็กซออเกต", "เอ็กซ์ออร์เกจ", "xor gate"],
		"category": "term",
	},
	{"right": "NOT Gate", "wrong": ["น็อตเกต", "นอตเกต", "น็อตเกจ", "not gate"], "category": "term"},
	{"right": "NAND Gate", "wrong": ["แนนด์เกต", "แนนเกต", "แนนด์เกจ", "nand gate"], "category": "term"},
	{"right": "NOR Gate", "wrong": ["นอร์เกต", "นอเกต", "นอร์เกจ", "nor gate"], "category": "term"},
	{
		"right": "Gate",
		"wrong": ["เกต", "เกจ", "เกด", "เกท", "เกตส์"],
		"category": "term",
		"note": "Logic gate — ไทยทับศัพท์เป็น เกต/เกจ/เกด",
	},
	# --- YOLO model sizes --------------------------------------------------
	{"right": "Nano", "wrong": ["นาโน", "นาโน่", "เนโน่"], "category": "term"},
	{"right": "Small", "wrong": ["สมอล", "สมอลล์", "สมอว์"], "category": "term"},
	{"right": "Medium", "wrong": ["มีเดียม", "เมเดียม", "มีเดี่ยม"], "category": "term"},
	{"right": "Large", "wrong": ["ลาร์จ", "ลาจ", "ล้าจ", "ลาช"], "category": "term"},
	{"right": "Extra Large", "wrong": ["เอ็กซ์ตร้าลาร์จ", "เอ็กซตราลาจ", "extra large"], "category": "term"},
	# --- Technical terms that should read as capitalised English -----------
	{"right": "Machine Learning", "wrong": ["แมชชีนเลิร์นนิง", "แมชชีนเลิร์นนิ่ง", "แมชีนเลิร์นนิ่ง", "แมชชีน เลิร์นนิ่ง"], "category": "english"},
	{"right": "Deep Learning", "wrong": ["ดีปเลิร์นนิง", "ดีปเลิร์นนิ่ง", "ดีป เลิร์นนิ่ง"], "category": "english"},
	{"right": "Neural Network", "wrong": ["นิวรอลเน็ตเวิร์ก", "นิวรัลเน็ตเวิร์ค", "นิวรอลเน็ตเวิร์ค", "นิวรัล เน็ตเวิร์ก"], "category": "english"},
	{"right": "Algorithm", "wrong": ["อัลกอริทึม", "อัลกอริธึม", "อัลกอริทึ่ม", "อัลกอฯ"], "category": "english"},
	{"right": "Perceptron", "wrong": ["เพอร์เซปตรอน", "เปอร์เซปตรอน", "เพอเซปตรอน", "เพอร์เซฟตรอน"], "category": "english"},
	{"right": "Notebook", "wrong": ["โน้ตบุ๊ก", "โน๊ตบุ๊ค", "โน้ตบุ๊ค", "โนตบุ๊ก"], "category": "english",
	 "note": "ในบริบทนี้หมายถึง Notebook ของ Colab/Jupyter ไม่ใช่โน้ตบุ๊กคอมพิวเตอร์"},
	{"right": "Working Directory", "wrong": ["เวิร์กกิงไดเรกทอรี", "เวิร์คกิ้งไดเรกทอรี", "working dir"], "category": "english"},
	{"right": "Directory", "wrong": ["ไดเรกทอรี", "ไดเรกทอรี่", "ไดเร็กทอรี"], "category": "english"},
	{"right": "Dataset", "wrong": ["ดาต้าเซต", "ดาต้าเซ็ต", "เดต้าเซ็ต", "ดาตาเซ็ต", "ดาต้า เซ็ต"], "category": "english"},
	{"right": "Epoch", "wrong": ["อีพ็อก", "เอป็อค", "อีพอค", "เอพ็อก"], "category": "english"},
	{"right": "Batch Size", "wrong": ["แบตช์ไซส์", "แบตช์ไซซ์", "แบชไซส์", "batch size"], "category": "english"},
	{"right": "Learning Rate", "wrong": ["เลิร์นนิงเรต", "เลิร์นนิ่งเรท", "learning rate"], "category": "english"},
	{"right": "Bounding Box", "wrong": ["บาวดิงบ็อกซ์", "บาวน์ดิ้งบ็อกซ์", "bounding box"], "category": "english"},
	{"right": "Confidence", "wrong": ["คอนฟิเดนซ์", "คอนฟิเด้นซ์", "คอนฟิเด้น"], "category": "english"},
	{"right": "Label", "wrong": ["เลเบล", "เลเบิล", "เลเบว"], "category": "english"},
	{"right": "Annotation", "wrong": ["แอนโนเทชัน", "แอนโนเทชั่น", "แอนโนเตชั่น"], "category": "english"},
	{"right": "Augmentation", "wrong": ["ออกเมนเทชัน", "ออกเมนเทชั่น", "อ็อกเมนเทชั่น"], "category": "english"},
	{"right": "Inference", "wrong": ["อินเฟอเรนซ์", "อินเฟอเร้นซ์", "อินเฟอเรน"], "category": "english"},
	{"right": "Object Detection", "wrong": ["ออบเจกต์ดีเทกชัน", "ออบเจคดีเทคชั่น", "อ็อบเจกต์ดีเทคชั่น", "object detection"], "category": "english"},
	{"right": "Classification", "wrong": ["คลาสสิฟิเคชัน", "คลาสสิฟิเคชั่น", "คลาสซิฟิเคชั่น"], "category": "english"},
	{"right": "Regression", "wrong": ["รีเกรสชัน", "รีเกรสชั่น", "รีเกรชชั่น"], "category": "english"},
	{"right": "Activation Function", "wrong": ["แอกทิเวชันฟังก์ชัน", "แอคทิเวชั่นฟังก์ชั่น", "activation function"], "category": "english"},
	{"right": "Loss Function", "wrong": ["ลอสฟังก์ชัน", "ลอสฟังก์ชั่น", "loss function"], "category": "english"},
	{"right": "Gradient Descent", "wrong": ["เกรเดียนต์ดีเซนต์", "เกรเดียนดีเซนต์", "gradient descent"], "category": "english"},
	{"right": "Backpropagation", "wrong": ["แบ็กพรอพาเกชัน", "แบ็คพรอพาเกชั่น", "back propagation"], "category": "english"},
	{"right": "Overfitting", "wrong": ["โอเวอร์ฟิตติง", "โอเวอร์ฟิตติ้ง", "over fitting"], "category": "english"},
	{"right": "Underfitting", "wrong": ["อันเดอร์ฟิตติง", "อันเดอร์ฟิตติ้ง", "under fitting"], "category": "english"},
	{"right": "Validation", "wrong": ["วาลิเดชัน", "วาลิเดชั่น", "แวลิเดชั่น"], "category": "english"},
	{"right": "Accuracy", "wrong": ["แอคคูราซี", "แอคคิวราซี่", "แอกคูเรซี"], "category": "english"},
	{"right": "Precision", "wrong": ["พรีซิชัน", "พรีซิชั่น", "พรีซิสชั่น"], "category": "english"},
	{"right": "Recall", "wrong": ["รีคอล", "รีคอลล์"], "category": "english"},
	{"right": "Confusion Matrix", "wrong": ["คอนฟิวชันเมทริกซ์", "คอนฟิวชั่นเมทริกซ์", "confusion matrix"], "category": "english"},
	{"right": "Feature", "wrong": ["ฟีเจอร์", "ฟีทเจอร์"], "category": "english"},
	{"right": "Weight", "wrong": ["เวต", "เวยท์", "เว้ยท์"], "category": "english"},
	{"right": "Bias", "wrong": ["ไบแอส", "ไบอัส", "ไบแอ็ส"], "category": "english"},
	{"right": "Layer", "wrong": ["เลเยอร์", "เลเย่อร์"], "category": "english"},
	{"right": "Tensor", "wrong": ["เทนเซอร์", "เท็นเซอร์"], "category": "english"},
	{"right": "Array", "wrong": ["อาเรย์", "แอเรย์", "อะเรย์"], "category": "english"},
	{"right": "Function", "wrong": ["ฟังก์ชัน", "ฟังก์ชั่น", "ฟังชั่น"], "category": "english"},
	{"right": "Variable", "wrong": ["แวริเอเบิล", "แวเรียเบิล", "ตัวแปรแวริเอเบิล"], "category": "english"},
	{"right": "Library", "wrong": ["ไลบรารี", "ไลบรารี่", "ไลบราลี่"], "category": "english"},
	{"right": "Framework", "wrong": ["เฟรมเวิร์ก", "เฟรมเวิร์ค", "เฟรมเวิค"], "category": "english"},
	{"right": "Runtime", "wrong": ["รันไทม์", "รันไทม", "run time"], "category": "english"},
	{"right": "GPU", "wrong": ["จีพียู", "จี พี ยู"], "category": "english"},
	{"right": "CPU", "wrong": ["ซีพียู", "ซี พี ยู"], "category": "english"},
	{"right": "API", "wrong": ["เอพีไอ", "เอ พี ไอ"], "category": "english"},
	{"right": "Parameter", "wrong": ["พารามิเตอร์", "พารามิเตอร์"], "category": "english"},
	{"right": "Threshold", "wrong": ["เทรชโฮลด์", "เทรสโฮลด์", "เธรชโฮลด์"], "category": "english"},
	{"right": "Pipeline", "wrong": ["ไปป์ไลน์", "ไพป์ไลน์"], "category": "english"},
	{"right": "Checkpoint", "wrong": ["เช็กพอยต์", "เช็คพอยท์", "check point"], "category": "english"},
	{"right": "Import", "wrong": ["อิมพอร์ต", "อิมพอร์ท"], "category": "english"},
	{"right": "Class", "wrong": ["คลาส", "คลาสส์"], "category": "english"},
	{"right": "Image", "wrong": ["อิมเมจ", "อิเมจ"], "category": "english"},
	{"right": "Video", "wrong": ["วิดีโอ", "วีดีโอ", "วีดิโอ"], "category": "english"},
]

# Words we deliberately keep as Thai transliterations, because that is how they are
# actually said in these videos. A protected term is matched by the same single pass
# and returned untouched, which also shields it from every other rule.
DEFAULT_PROTECTED: list[str] = [
	"โมเดล",
	"โมดูล",
	"เทรน",
	"เทรนนิ่ง",
	"เทรนนิง",
	"เซฟ",
	"พาธ",
	"แพทเทิร์น",
	"โค้ด",
	"ไฟล์",
	"โฟลเดอร์",
	"รัน",
	"คลิก",
	"ดาวน์โหลด",
	"อัปโหลด",
	"เบราว์เซอร์",
]


def _slugify(value: str) -> str:
	"""Stable, readable rule id derived from the canonical spelling."""
	normalized = unicodedata.normalize("NFKC", value).strip().lower()
	slug = re.sub(r"[^a-z0-9฀-๿]+", "-", normalized).strip("-")
	if slug:
		return slug
	# Punctuation-only canonical forms ("!", "?", "#") slug to nothing, so fall back
	# to code points. A shared fallback would silently collapse those rules into one.
	return "-".join(f"u{ord(char):04x}" for char in normalized) or "rule"


@dataclass
class Rule:
	right: str
	wrong: list[str]
	category: str = "term"
	note: str = ""
	id: str = ""
	source: str = "builtin"
	enabled: bool = True

	def __post_init__(self) -> None:
		if not self.id:
			self.id = f"{self.category}:{_slugify(self.right)}"

	def to_dict(self) -> dict:
		return {
			"id": self.id,
			"right": self.right,
			"wrong": list(self.wrong),
			"category": self.category,
			"category_label": CATEGORY_LABELS.get(self.category, self.category),
			"note": self.note,
			"source": self.source,
			"enabled": self.enabled,
		}


def _expand_variants(rule: Rule) -> list[str]:
	"""Variants to match for a rule, including the canonical form itself.

	The canonical form is registered as a self-match so that a *correctly* spelled
	occurrence consumes its own text and cannot be partly rewritten by a shorter,
	unrelated variant. For an ASCII canonical we also register its lower-cased form,
	which is what normalises casing: matching is case-insensitive, so any casing of
	"machine learning" resolves to "Machine Learning".
	"""
	variants = list(rule.wrong)
	variants.append(rule.right)
	if _is_ascii(rule.right):
		variants.append(rule.right.lower())
	# De-duplicate case-insensitively while keeping declaration order.
	seen: set[str] = set()
	unique: list[str] = []
	for variant in variants:
		variant = variant.strip()
		key = variant.lower()
		if variant and key not in seen:
			seen.add(key)
			unique.append(variant)
	return unique


def _variant_pattern(variant: str) -> str:
	escaped = re.escape(variant)
	if _is_ascii(variant):
		# Keep "data.yml" from matching inside "mydata.yml2", and "Play" from
		# matching inside "Player".
		return rf"(?<![A-Za-z0-9_]){escaped}(?![A-Za-z0-9_])"
	# Thai has no word delimiter to anchor on, so the only reliable guard is that we
	# must not stop in the middle of a syllable: reject a following vowel/tone mark.
	# This is what keeps "เกต" from matching inside "เกตุ".
	return rf"{escaped}(?![{_THAI_COMBINING}])"


@dataclass
class Glossary:
	"""A compiled word system. Build it with :func:`build_glossary`."""

	rules: list[Rule] = field(default_factory=list)
	protected: list[str] = field(default_factory=list)
	_pattern: re.Pattern | None = field(default=None, repr=False)
	_lookup: dict[str, tuple[str, str, str]] = field(default_factory=dict, repr=False)

	def compile(self) -> "Glossary":
		lookup: dict[str, tuple[str, str, str]] = {}
		variants: list[str] = []

		# Protected terms are registered first so that a rule cannot claim the same
		# spelling: the word we promised to keep in Thai always wins a tie.
		for term in self.protected:
			term = term.strip()
			if not term:
				continue
			key = term.lower()
			if key not in lookup:
				lookup[key] = (term, "protected", "protected")
				variants.append(term)

		for rule in self.rules:
			if not rule.enabled:
				continue
			for variant in _expand_variants(rule):
				key = variant.lower()
				if key in lookup:
					continue
				lookup[key] = (rule.right, rule.category, rule.id)
				variants.append(variant)

		if not variants:
			self._pattern = None
			self._lookup = {}
			return self

		# Longest first: Python's alternation is leftmost-first-alternative, so this
		# ordering is what gives us longest-match semantics ("Roboflow Universe"
		# before "Roboflow", "แอนด์เกต" before "เกต").
		variants.sort(key=lambda variant: (-len(variant), variant))
		self._pattern = re.compile(
			"|".join(_variant_pattern(variant) for variant in variants),
			re.IGNORECASE,
		)
		self._lookup = lookup
		return self

	def correct(self, text: str) -> tuple[str, list[dict]]:
		"""Repair `text` in a single pass.

		Returns the corrected text and one record per replacement, each carrying the
		original spelling, the canonical spelling, and the rule that fired — the data
		the UI highlights.
		"""
		if not text or self._pattern is None:
			return text, []

		corrections: list[dict] = []

		def _replace(match: re.Match) -> str:
			matched = match.group(0)
			entry = self._lookup.get(matched.lower())
			if entry is None:
				return matched
			right, category, rule_id = entry
			# Protected terms and already-correct spellings consume their text
			# without being reported as a change.
			if category == "protected" or matched == right:
				return matched
			corrections.append(
				{
					"before": matched,
					"after": right,
					"category": category,
					"category_label": CATEGORY_LABELS.get(category, category),
					"rule_id": rule_id,
				}
			)
			return right

		return self._pattern.sub(_replace, text), corrections

	def rule_dicts(self) -> list[dict]:
		return [rule.to_dict() for rule in self.rules]

	def stats(self) -> dict:
		return {
			"rule_count": sum(1 for rule in self.rules if rule.enabled),
			"variant_count": len(self._lookup),
			"protected_count": len(self.protected),
		}


def build_glossary(
	extra_rules: list[dict] | None = None,
	disabled_rule_ids: set[str] | None = None,
	extra_protected: list[str] | None = None,
) -> Glossary:
	"""Compile the built-in word system plus any user-stored additions.

	`extra_rules` entries with an id that already exists replace the built-in rule,
	which is how a user overrides a default instead of fighting with it.
	"""
	disabled_rule_ids = disabled_rule_ids or set()

	rules: dict[str, Rule] = {}
	for raw in DEFAULT_RULES:
		rule = Rule(source="builtin", **raw)
		if rule.id in rules:
			# Two built-ins sharing an id would silently drop one of them, so this is
			# a hard error rather than a warning.
			raise ValueError(
				f"Duplicate built-in glossary rule id {rule.id!r} "
				f"({rules[rule.id].right!r} vs {rule.right!r})"
			)
		rule.enabled = rule.id not in disabled_rule_ids
		rules[rule.id] = rule

	for raw in extra_rules or []:
		rule = Rule(
			right=str(raw["right"]).strip(),
			wrong=[str(item).strip() for item in raw.get("wrong", []) if str(item).strip()],
			category=str(raw.get("category") or "term"),
			note=str(raw.get("note") or ""),
			id=str(raw.get("id") or ""),
			source=str(raw.get("source") or "user"),
		)
		rule.enabled = rule.id not in disabled_rule_ids
		rules[rule.id] = rule

	protected = list(DEFAULT_PROTECTED)
	for term in extra_protected or []:
		term = str(term).strip()
		if term and term not in protected:
			protected.append(term)

	return Glossary(rules=list(rules.values()), protected=protected).compile()


# A default instance so callers that do not need DB-backed overrides stay cheap.
_default_glossary: Glossary | None = None


def default_glossary() -> Glossary:
	global _default_glossary
	if _default_glossary is None:
		_default_glossary = build_glossary()
	return _default_glossary
