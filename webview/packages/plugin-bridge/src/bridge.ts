import type {
  AppCommand,
  EntitySelection,
  OpDispatch,
  ViewportInput,
} from './wire';
import type { FrontendState } from './state';

/**
 * Async-request kinds a panel may round-trip through the bridge. Keep in sync
 * with `foldit_gui::bridge::message::RequestKind` (snake_case discriminants on
 * the wire).
 */
export type RequestKind =
  | 'read_resource_file'
  | 'server_request'
  | 'get_hotkey_text'
  | 'panels_catalog'
  | 'settings_catalog';

/**
 * An `OpDispatch` with `focused_entity_id` omittable: a missing key
 * deserializes to `None` on the Rust side, so click-to-fire buttons can
 * post `{ op_id }` alone.
 */
export type DispatchableOp = Omit<OpDispatch, 'focused_entity_id'> & {
  focused_entity_id?: OpDispatch['focused_entity_id'];
};

/**
 * Framework-neutral host surface handed to a plugin panel. Carries the
 * read side (state subscription + snapshot) and the write side (the same
 * IPC commands the native chrome uses), plus the contract version a plugin
 * checks against at mount.
 */
export type PluginBridge = {
  /**
   * Subscribe to raw section-keyed state deltas. With `selector`, the
   * callback only fires when a delta touches one of the named sections.
   * Returns an unsubscribe fn.
   */
  subscribe(
    cb: (delta: Partial<FrontendState>) => void,
    selector?: (keyof FrontendState)[],
  ): () => void;
  /** Deep copy of the current accumulated state. Safe to mutate. */
  snapshot(): FrontendState;
  dispatchOp: (op: DispatchableOp) => void;
  appCommand: (command: AppCommand) => void;
  setSelection: (entries: EntitySelection[]) => void;
  viewportInput: (input: ViewportInput) => void;
  request: <T = unknown>(kind: RequestKind, payload?: object, timeoutMs?: number) => Promise<T>;
  openSessionDialog: () => void;
  readonly contractVersion: number;
};

/**
 * A plugin panel's entry point: render into `shadow`, return a cleanup fn.
 * `panelId` lets one entry point serve several declared panels.
 */
export type MountPanel = (
  panelId: string,
  shadow: ShadowRoot,
  bridge: PluginBridge,
) => () => void;
