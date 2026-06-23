/**
 * SolidJS State Hooks — Tauri Adapter Bridge
 *
 * Provides the same hook API that components expect, backed by the Tauri
 * adapter's reactive SolidJS store instead of Zustand vanilla stores.
 *
 * Most game state is backend-driven (score, selection, actions, loading,
 * panel visibility, game progress, segment info) and arrives through the
 * Tauri state-update events as read-only mirrors. Only genuinely GUI-local
 * state (the dragged text-bubble position, behavior options) lives in
 * SolidJS signals here.
 */

import { createSignal } from 'solid-js';
import { state, appCommand } from '../adapter';

/**
 * Make a function work both as a direct call and as an accessor-unwrap call.
 * - With args: `fn(arg)` → executes immediately
 * - Without args: `fn()` → returns the function itself (for `fn()(arg)` pattern)
 *
 * This bridges the two hook usage patterns:
 * - Non-selector: `const { action } = useHook(); action(arg);`
 * - Selector: `const action = useHook(s => s.action); action()(arg);`
 */
function dual<F extends (...args: any[]) => any>(fn: F): F {
	return ((...args: any[]) => args.length ? fn(...args) : fn) as any;
}

// ============================================================================
// App screen derivation from loading state
// ============================================================================

export type AppScreen =
	| 'DOWNLOADING'
	| 'INITIALIZING'
	| 'LOADING_SESSION'
	| 'LANDING'
	| 'IN_SESSION'
	| 'BACKEND_ERROR';

/**
 * Derive the current app screen from the top-level `app_state` enum.
 * `app_state` is the single source of truth, owned by the Rust backend:
 * defaults to `'initializing'` on JS startup and stays on the loading
 * screen through the pre-session phases (`downloading` / `initializing`
 * / `loading_session`); `landing` shows the menu, and the screen flips
 * to the session UI once `in_session`.
 */
function deriveAppScreen(): AppScreen {
	switch (state.app_state) {
		case 'in_session':
			return 'IN_SESSION';
		case 'landing':
			return 'LANDING';
		case 'downloading':
		case 'initializing':
		case 'loading_session':
		default:
			return 'LOADING_SESSION';
	}
}

// ============================================================================
// GUI-only local state
// ============================================================================

// Bubble drag position (GUI-local). `null` = use the component's default
// anchor (20%/20% of viewport); set by the drag handler in TextBubble.tsx.
// Persists across bubbles so a user-dragged position carries over to the
// next bubble in the sequence (matches original Foldit behavior).
const [localTextBubblePosition, setLocalTextBubblePosition] =
	createSignal<{ x: number; y: number } | null>(null);

// Panel open/closed state is backend-authoritative: a panel is open iff its
// name is in `state.panels.open`. The helpers send `SetPanelVisible` commands
// and read the projected set, so toggling round-trips through a backend tick.
function toggleWidget(name: string) {
	appCommand({ type: 'SetPanelVisible', panel: name, visible: !isWidgetVisible(name) });
}

function showWidget(name: string) {
	appCommand({ type: 'SetPanelVisible', panel: name, visible: true });
}

function hideWidget(name: string) {
	appCommand({ type: 'SetPanelVisible', panel: name, visible: false });
}

export function isWidgetVisible(name: string): boolean {
	return state.panels?.open?.includes(name) ?? false;
}

// Camel-case a kebab-case panel descriptor id into the `state.panels.open`
// key. Both the panel chrome and the launcher toggle must derive the same
// key from the same id, or a launcher flips a key the panel never reads.
export function panelVisibilityKey(id: string): string {
	return id.replace(/-(\w)/g, (_, c) => c.toUpperCase());
}

// ============================================================================
// Hook: useFrontendState()
// ============================================================================

// Backend's `puzzle.mode` is a lowercase tag ("game" / "scientist"). The
// rest of the codebase keys off the GameMode enum string values
// ("education" / "campaign" / "scientist" / "science_puzzle"), so we map
// the backend's coarse two-mode signal to the legacy 4-mode taxonomy.
//
// Game mode → EDUCATION (intro/tutorial puzzles drive the bulk of game-mode
// usage today; campaign/science_puzzle differentiation will come from
// elsewhere when we plumb server puzzles).
function deriveGameMode(): 'education' | 'campaign' | 'scientist' | 'science_puzzle' {
	return state.puzzle.mode === 'game' ? 'education' : 'scientist';
}

function _buildFrontendState() {
	return {
		appScreen: () => deriveAppScreen(),
		gameMode: () => deriveGameMode(),
		downloadProgress: () => (state.loading.progress ?? 0) * 100,
		// Backend-authoritative fullscreen flag. The toggle is driven by the
		// LogoDropdown click handler (DOM fullscreen call in-gesture plus a
		// SetFullscreen command); this only reads the projected mirror.
		fullscreen: () => state.ui.fullscreen,
		// Query helpers
		inGame: () => deriveAppScreen() === 'IN_SESSION',
		isLoading: () => ['DOWNLOADING', 'INITIALIZING', 'LOADING_SESSION'].includes(deriveAppScreen()),
		isIntro: () => deriveGameMode() === 'education' || deriveGameMode() === 'campaign',
		isSciencePuzzle: () => deriveGameMode() === 'science_puzzle',
		isScientist: () => deriveGameMode() === 'scientist',
		// Actions
		setAppScreen: dual((_screen: AppScreen) => { /* Controlled by backend loading state */ }),
	};
}
type FrontendStateResult = ReturnType<typeof _buildFrontendState>;

export function useFrontendState(): FrontendStateResult;
export function useFrontendState<T>(selector: (state: FrontendStateResult) => T): T;
export function useFrontendState(selector?: (s: any) => any) {
	const data = _buildFrontendState();
	return selector ? selector(data) : data;
}

// ============================================================================
// Hook: useBackendData()
// ============================================================================

function _buildBackendData() {
	return {
		// Score
		currentScore: () => state.score.value,
		invalidScore: () => state.score.invalid,
		// Selection — list of per-entity residue selections; entities with
		// no selected residues are absent from `entries`.
		selection: () => state.selection.entries,
		// FPS
		fps: () => state.ui.fps,
		alignmentData: () => null as null | { sequence?: unknown[] },
		// Actions
		actions: () => state.actions.available,
		// Per-plugin group metadata, joined to `actions` on `plugin_id`.
		actionGroups: () => state.actions.groups,
		// Helpers
		hasSelection: () => state.selection.entries.some(e => e.residues.length > 0),
		hasActiveAction: () => state.actions.available.some(a => a.active),
		sceneEntities: () => state.scene?.entities ?? [],
		// Backend-driven puzzle context (mode + scores + title).
		puzzleData: () => ({
			puzzleId: state.puzzle.puzzle_id,
			title: state.puzzle.title,
			startingScore: state.puzzle.starting_score,
			targetScore: state.puzzle.target_score,
			actions: [],
		}),
		// Backend latches `puzzle.complete` once current_score crosses the
		// game target; the consumer (PuzzleComplete widget effect) opens the
		// victory modal on the false→true transition.
		puzzleComplete: () => state.puzzle.complete,
		segmentInfo: () => state.segment_info ?? null,
		backendErrorMessage: () => null,
		undoHistory: () => ({ canUndo: false, canRedo: false }),
		log: () => state.ui.log ?? '',
		selectedCount: () => state.ui.selected_count ?? 0,
	};
}
type BackendDataResult = ReturnType<typeof _buildBackendData>;

export function useBackendData(): BackendDataResult;
export function useBackendData<T>(selector: (state: BackendDataResult) => T): T;
export function useBackendData(selector?: (s: any) => any) {
	const data = _buildBackendData();
	return selector ? selector(data) : data;
}

// ============================================================================
// Hook: useUI()
// ============================================================================

function _buildUI() {
	return {
		// Text bubble from backend
		textBubble: () => state.ui.text_bubble,
		textBubblePosition: () => localTextBubblePosition(),
		setTextBubblePosition: dual((pos: { x: number; y: number } | null) =>
			setLocalTextBubblePosition(pos)
		),
		// Hints visibility (backend-authoritative via state.ui.hints_visible).
		// Toggled by LogoDropdown's hints item through a SetHintsVisible
		// command; gates whether the TextBubble widget renders the payload.
		hintsVisible: () => state.ui.hints_visible,
		toggleHints: dual(() => appCommand({ type: 'SetHintsVisible', visible: !state.ui.hints_visible })),
		setHintsVisible: dual((v: boolean) => appCommand({ type: 'SetHintsVisible', visible: v })),
		// Panel visibility accessors with a direct reader outside state.ts.
		// Every other panel reads visibility through Panel.tsx's
		// isWidgetVisible(camelCase(id)) path, so they need no accessor here.
		puzzleMenu: () => isWidgetVisible('puzzleMenu'),
		loginMenu: () => isWidgetVisible('loginMenu'),
		logoDropdown: () => isWidgetVisible('logoDropdown'),
		puzzleCompleteMenu: () => isWidgetVisible('puzzleCompleteMenu'),
		// Rising text
		risingTextMessages: () => [] as any[],
		// Language
		preferredLanguage: () => 'en',
		// Actions
		toggleWidget: dual(toggleWidget),
		showWidget: dual(showWidget),
		hideWidget: dual(hideWidget),
		resetWidgets: dual(() => {
			for (const name of state.panels?.open ?? []) {
				hideWidget(name);
			}
		}),
		removeRisingText: dual((_id: string) => {}),
		setPreferredLanguage: dual((_lang: string) => {}),
	};
}
type UIResult = ReturnType<typeof _buildUI>;

export function useUI(): UIResult;
export function useUI<T>(selector: (state: UIResult) => T): T;
export function useUI(selector?: (s: any) => any) {
	const data = _buildUI();
	return selector ? selector(data) : data;
}

// ============================================================================
// Hook: useGameProgress()
// ============================================================================

// Backend-authoritative puzzle high-score progress: `state.progress.entries`
// is the best display score recorded per puzzle id, projected each tick. A
// puzzle absent from the map has never been scored. The high-score lookup
// keys by string id (the menu passes both number and string ids); a puzzle
// counts as complete once its best is positive.
function highScoreFor(id: number | string): number {
	const key = Number(id);
	const entry = state.progress?.entries?.find((e) => e.puzzle_id === key);
	return entry?.high_score ?? 0;
}

function _buildGameProgress() {
	return {
		isPuzzleComplete: dual((id: number | string) => highScoreFor(id) > 0),
		getHighScore: dual((id: number | string) => highScoreFor(id)),
		clearProgress: dual(() => {
			appCommand({ type: 'ClearProgress' });
		}),
	};
}
type GameProgressResult = ReturnType<typeof _buildGameProgress>;

export function useGameProgress(): GameProgressResult;
export function useGameProgress<T>(selector: (state: GameProgressResult) => T): T;
export function useGameProgress(selector?: (s: any) => any) {
	const data = _buildGameProgress();
	return selector ? selector(data) : data;
}

// ============================================================================
// Hook: useBackendOptions()
// ============================================================================

const [behaviorOptions, setBehaviorOptions] = createSignal({
	clashing: 1.0,
	sidechain: 1.0,
	backbone: 1.0,
	wigglePower: 1.0,
	enableCutBands: false,
});

function _buildBackendOptions() {
	return {
		behaviorOptions: () => behaviorOptions(),
		exploreMode: () => false,
		setBehaviorOptions: dual((opts: any) => setBehaviorOptions(opts)),
	};
}
type BackendOptionsResult = ReturnType<typeof _buildBackendOptions>;

export function useBackendOptions(): BackendOptionsResult;
export function useBackendOptions<T>(selector: (state: BackendOptionsResult) => T): T;
export function useBackendOptions(selector?: (s: any) => any) {
	const data = _buildBackendOptions();
	return selector ? selector(data) : data;
}

// ============================================================================
// Direct state access for non-component code
// ============================================================================

export const getUIState = () => ({
	textBubble: state.ui.text_bubble,
	toggleWidget,
	showWidget,
	hideWidget,
	isWidgetVisible,
});

export const getBackendData = () => ({
	currentScore: state.score.value,
	invalidScore: state.score.invalid,
	selection: state.selection.entries,
	actions: state.actions.available,
});

export const getFrontendState = () => ({
	appScreen: deriveAppScreen(),
	gameMode: 'scientist' as const,
	inGame: deriveAppScreen() === 'IN_SESSION',
});

export const getBackendOptions = () => ({
	behaviorOptions: behaviorOptions(),
});

export const hasSelection = (): boolean => state.selection.entries.some(e => e.residues.length > 0);
export const hasActiveAction = (): boolean => state.actions.available.some(a => a.active);
