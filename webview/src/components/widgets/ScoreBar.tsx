/**
 * ScoreBar Component
 *
 * SolidJS score bar component with gradient colors and progress display.
 */

import { Component, createMemo, createEffect, Show } from 'solid-js';
import { Info } from '../../utils/iconMapping';
import '../../styles/widgets/ScoreBar.css';
import { useBackendOptions, useBackendData, useFrontendState } from '../../services/adapters';

const ScoreBar: Component = () => {
  const exploreMode = useBackendOptions(state => state.exploreMode);
  const currentScore = useBackendData(state => state.currentScore);
  const invalidScore = useBackendData(state => state.invalidScore);
  const puzzleData = useBackendData(state => state.puzzleData);
  const gameMode = useFrontendState(state => state.gameMode);

  const isIntro = createMemo(() =>
    gameMode() === 'education' || gameMode() === 'campaign'
  );

  const title = createMemo(() => puzzleData()?.title ?? '');

  // Memoize progress calculation
  const progress = createMemo(() => {
    const data = puzzleData();
    if (!data) return 0;
    const current = currentScore();
    const range = data.targetScore - data.startingScore;
    if (range === 0) return 0;
    const percent = (current - data.startingScore) * 100.0 / range;
    return Math.max(0, Math.min(100, percent));
  });

  // Update CSS variables when progress changes
  createEffect(() => {
    // In sandbox mode, use 50% (amber/yellow) since there's no target score
    const p = isIntro() ? progress() : 50;

    const fromRed = Math.min(32, 32 * (100 - p) / 100.0);
    const viaRed = Math.min(128, 128 * (100 - p) / 100.0);
    const toRed = Math.min(255, 255 * (100 - p) / 100.0);

    const fromGreen = Math.min(32, 32 * p / 100.0);
    const viaGreen = Math.min(128, 128 * p / 100.0);
    const toGreen = Math.min(255, 255 * p / 100.0);

    document.documentElement.style.setProperty('--from-color', `rgb(${fromRed}, ${fromGreen}, 0)`);
    document.documentElement.style.setProperty('--via-color', `rgb(${viaRed}, ${viaGreen}, 0)`);
    document.documentElement.style.setProperty('--to-color', `rgb(${toRed}, ${toGreen}, 0)`);
  });

  const toSnakeCase = (str: string) => str.replace(/ /g, '_');

  // In sandbox mode, link to RCSB; in intro mode, link to Foldit wiki
  const titleHref = createMemo(() => {
    const t = title();
    if (!t) return '';
    if (isIntro()) {
      return `https://foldit.fandom.com/wiki/Education/${toSnakeCase(t)}`;
    }
    return `https://www.rcsb.org/structure/${t}`;
  });

  return (
    <div id="score-bar-container" class="score-bar-container"
      style={{ "font-family": "'Noto Sans', sans-serif", "letter-spacing": "1px" }}>

      <Show when={title()}>
        <div class="puzzle-title-icons">
          <h2>{title()}</h2>
          <a href={titleHref()}
            target="_blank" rel="noopener noreferrer" class="cursor-pointer pointer-events-auto">
            <Info size={24} />
          </a>
        </div>
      </Show>

      <Show when={exploreMode()}>
        <div id="explore-bar" class="explore-mode-text">Explore Mode</div>
      </Show>

      <Show when={!exploreMode()}>
        <div id="score-bar" class="score-bar-white-bg">
          <div
            class="score-bar-color-fg"
            style={{ width: `${isIntro() ? progress() : 100}%` }}
          />

          <div class="score-text">
            {currentScore().toFixed(2)}
            {isIntro() && puzzleData() && ` / ${puzzleData().targetScore}`}
          </div>

          <Show when={invalidScore()}>
            <div class="score-strikethrough" />
          </Show>
        </div>
      </Show>
    </div>
  );
};

export default ScoreBar;
