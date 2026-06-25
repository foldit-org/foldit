/**
 * Custom SolidJS slider with draggable thumb and visual track fill.
 */

import { Component, createSignal } from 'solid-js';
import '../../styles/util/slider.css';

export interface SliderProps {
  min: number;
  max: number;
  step?: number;
  value: number;
  onChange: (value: number) => void;
}

const Slider: Component<SliderProps> = (props) => {
  let trackRef: HTMLDivElement | undefined;
  const [dragging, setDragging] = createSignal(false);

  const clampAndSnap = (raw: number): number => {
    const step = props.step ?? 0.01;
    const clamped = Math.min(props.max, Math.max(props.min, raw));
    return Math.round(clamped / step) * step;
  };

  const fraction = () => {
    const range = props.max - props.min;
    if (range <= 0) return 0;
    return ((props.value ?? props.min) - props.min) / range;
  };

  const valueFromPointer = (clientX: number): number => {
    if (!trackRef) return props.min;
    const rect = trackRef.getBoundingClientRect();
    const ratio = (clientX - rect.left) / rect.width;
    return clampAndSnap(props.min + ratio * (props.max - props.min));
  };

  const onPointerDown = (e: PointerEvent) => {
    e.preventDefault();
    setDragging(true);
    props.onChange(valueFromPointer(e.clientX));
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  };

  const onPointerMove = (e: PointerEvent) => {
    if (!dragging()) return;
    props.onChange(valueFromPointer(e.clientX));
  };

  const onPointerUp = () => {
    setDragging(false);
  };

  return (
    <div
      ref={trackRef}
      class="slider-container"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
    >
      <div
        class="slider-progress"
        style={{ width: `${fraction() * 100}%` }}
      />
      <div
        class="slider-thumb"
        style={{ left: `${fraction() * 100}%` }}
      />
    </div>
  );
};

export default Slider;
