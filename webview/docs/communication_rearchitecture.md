# Proposed Foldit GUI <-> Backend Communication Re-Architecture

This document outlines a plan to restructure the communication between the Foldit GUI (TypeScript) and Backend (C++) using FlatBuffers schemas and a consolidated type system.

## Core Motivation
*   **Performance:** Replace slow JSON serialization with efficient binary schemas (FlatBuffers).
*   **Maintainability:** Establish a single source of truth for data structures, eliminating "stringly-typed" bugs.
*   **Organization:** Move from flat, ad-hoc enums to structured, logical categories of messages.
*   **Scalability:** Allow new message types to be added via Unions without exploding the top-level API.

## The "Union" Pattern
Instead of separate functions/enums for every single event, we define high-level "Archetype" messages that contain a `union` of specific payload types.

### 1. Backend to GUI (B -> G)
*Direction: C++ sends state updates to JS.*

Current Count: ~16 Enums
Proposed Top-Level Types: 4

#### A. `GameCoreUpdate`
**Purpose:** High-frequency or essential game loop updates.
*   `SELECT_PUZZLE` -> `PuzzleInfo`
*   `ACTION` -> `UndoHistoryState`
*   `UNDO_MENU` -> `UndoStackDetails`
*   `GUI_DEBUG_PRINT` -> `DebugMessage`

#### B. `PanelDataUpdate`
**Purpose:** Heavy, on-demand data for specific analysis panels.
*   `RAMA` -> `RamaMapData`
*   `BLUEPRINT` -> `BlueprintData`
*   `ALIGNMENT` -> `AlignmentData`
*   `ROTAMER_PICKER` -> `RotamerList`
*   `ELECTRON_DENSITY` -> `DensityGrid`

#### C. `UIConfigUpdate`
**Purpose:** Changes to the HUD, overlay feedback, or input modes.
*   `OPTIONS` -> `GameOptions`
*   `TOGGLE_HINTS` -> `HintState`
*   `TEXT_BUBBLE` -> `BubbleMessage`
*   `TEXT_BUBBLE_TARGET` -> `BubbleTarget`
*   `GUI_INPUT_CAPTURE` -> `InputCaptureCmd`

#### D. `FileOperationStatus`
**Purpose:** Results of asynchronous I/O.
*   `SAVE` -> `SaveResult`
*   `LOAD` -> `LoadResult`

---

### 2. GUI to Backend (G -> B)
*Direction: JS sends user input or queries to C++.*

Current Count: ~9 Enums
Proposed Top-Level Types: 4

#### A. `UserInput` (Open Loop / Fire-and-Forget)
**Purpose:** Raw input or game actions. High frequency.
*   `MOUSE_INPUT` -> `MouseEvent`
*   `KEY_INPUT` -> `KeyEvent`
*   `SELECTION` -> `SelectionCmd` (User selecting atoms)

#### B. `SolutionQuery` (Closed Loop / Request-Response)
**Purpose:** Fetching puzzle solution data.
*   `LIST_SOLUTIONS` -> `ListSolutionsCmd`
*   `GET_SOLUTION_METADATA` -> `GetSolutionMetaCmd`
*   `GET_SOLUTION_MODEL` -> `GetSolutionModelCmd`

#### C. `ResourceQuery` (Closed Loop)
**Purpose:** Fetching static assets or text.
*   `GET_HOTKEY_TEXT` -> `GetHotkeyCmd`
*   `READ_RESOURCE_FILE` -> `ReadFileCmd`

#### D. `ServerApiRequest` (Closed Loop)
**Purpose:** Interaction with the Foldit Web API.
*   `SERVER_REQUEST` -> **Union of:**
    *   `LoginRequest`
    *   `SubmitScoreRequest`
    *   `GetLeaderboardRequest`
    *   ...and others.

---

## Implementation Strategy

1.  **Schema Definition (`game_protocol.fbs`):**
    Create the `.fbs` file defining all `tables` and `unions` listed above.

2.  **Code Generation:**
    Use `flatc` to generate `GameProtocol.ts` (for GUI) and `GameProtocol.h` (for Backend).

3.  **Transport Layer Update:**
    *   **C++:** Replace `json.dump()` with `FlatBufferBuilder`. Send `(pointer, length)`.
    *   **TS:** Replace `JSON.parse()` with `GameProtocol.ServerMessage.getRootAsServerMessage(new ByteBuffer(data))`.

4.  **Migration:**
    *   Start with the heaviest data (`PanelDataUpdate` - specifically Electron Density) to get immediate performance wins.
    *   Migrate `UserInput` next for responsiveness.
    *   Migrate lower-frequency events (Chat, Options) last.
