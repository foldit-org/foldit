// Plugin-facing slice of the generated wire contract.
//
// The generator output stays host-owned at `webview/src/generated/wire.ts`
// for now; this workspace-relative re-export is type-only (erased at build)
// plus the one runtime constant the version handshake needs.

export { BRIDGE_CONTRACT_VERSION } from '../../../src/generated/wire';

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
} from '../../../src/generated/wire';
