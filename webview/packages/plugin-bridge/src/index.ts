// Plugin-facing contract for @foldit panel bundles. Types + the version
// constant only; the runtime bridge and transport stay host-side.

export type { DispatchableOp, MountPanel, PluginBridge, RequestKind } from './bridge';

export type {
  FrontendState,
  LoadingSection,
  PuzzleSection,
  SceneSection,
  ScoreSection,
  UISection,
} from './state';

export { BRIDGE_CONTRACT_VERSION } from './wire';

export type {
  ActionInfo,
  ActionsSection,
  AppCommand,
  EntitySelection,
  OpDispatch,
  PanelInfo,
  ParamConstraint,
  ParamSpec,
  ParamType,
  ParamValue,
  SceneEntityInfo,
  SegmentInfo,
  ViewportInput,
} from './wire';
