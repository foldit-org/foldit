// Type definitions matching Rust FrontendState.
//
// Wire payload types come from the generated `./generated/wire` module
// (produced by the Rust `export_wire_types` test); hand-authoring a wire
// payload is a review red flag, fix the Rust struct and re-export. The
// composite `FrontendState` and its hand-authored sections live in
// `@foldit/plugin-bridge` (the plugin-facing contract) and are re-exported
// here so host imports resolve unchanged.

export type {
  ActionInfo,
  ActionsSection,
  AppCommand,
  AppPhase,
  CheckpointInfo,
  CheckpointKindTag,
  EntityId,
  EntitySelection,
  FilterStatus,
  HistoryCommand,
  HistoryLiveUpdate,
  HistorySection,
  OpDispatch,
  ParamConstraint,
  ParamSpec,
  ParamType,
  ParamValue,
  PanelInfo,
  PanelPosition,
  PanelsSection,
  PluginGroupInfo,
  ProgressEntry,
  ProgressSection,
  SceneEntityInfo,
  SegmentInfo,
  SelectionSection,
  SettingsTabInfo,
  ViewSection,
  ViewportInput,
} from './generated/wire';

export type {
  FrontendState,
  LoadingSection,
  PuzzleSection,
  SceneSection,
  ScoreSection,
  UISection,
} from '@foldit/plugin-bridge';
