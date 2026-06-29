// The read-model a plugin panel subscribes to: the composite
// `FrontendState` and its hand-authored sections. Sections backed by a Rust
// struct come from the generated wire module; the rest are defined here.

import type {
  ActionsSection,
  AppPhase,
  HistoryLiveUpdate,
  HistorySection,
  PanelsSection,
  ProgressSection,
  SceneEntityInfo,
  SegmentInfo,
  SelectionSection,
  ViewSection,
} from '../../../src/generated/wire';
import type { TextBubbleType } from '../../../src/models/UI';

export interface ScoreSection {
  value: number;
  invalid: boolean;
  title: string;
}

/** Backend-driven puzzle context. `mode` is the active scoring mode tag. */
export interface PuzzleSection {
  mode: 'game' | 'scientist';
  puzzle_id: number;
  title: string;
  starting_score: number;
  target_score: number;
  /** Latches true when the score crosses the target in `game` mode. */
  complete: boolean;
}

export interface UISection {
  text_bubble: TextBubbleType | null;
  fps: number;
  log: string;
  selected_count: number;
  /** Whether the tutorial-hint bubble is shown. Backend-authoritative;
   *  defaults true. */
  hints_visible: boolean;
  /** Whether the window is in OS fullscreen. Backend-authoritative mirror;
   *  desktop applies it to the native window, web drives the DOM API. */
  fullscreen: boolean;
}

export interface LoadingSection {
  progress: number | null;
  puzzle_loaded: boolean;
}

export interface SceneSection {
  entities: SceneEntityInfo[];
  /** Currently-focused entity id, or null for whole-session focus. */
  focused_entity: number | null;
}

export interface FrontendState {
  app_state: AppPhase;
  score: ScoreSection;
  puzzle: PuzzleSection;
  selection: SelectionSection;
  view: ViewSection;
  ui: UISection;
  actions: ActionsSection;
  loading: LoadingSection;
  scene: SceneSection;
  history: HistorySection;
  /** Optional small patch payload for the running tentative checkpoint. */
  history_live: HistoryLiveUpdate | null;
  /** Per-residue segment-info payload, present only while a residue is targeted. */
  segment_info?: SegmentInfo | null;
  /** Backend-authoritative panel open/closed set and per-panel positions. */
  panels: PanelsSection;
  /** Backend-authoritative puzzle high-score progress (best score per puzzle). */
  progress: ProgressSection;
}
