/**
 * SolidJS button component with various styling options.
 */

import { Component } from 'solid-js';
import '../../styles/util/buttons.css';
import '../../styles/util/effects.css';

export interface ButtonProps {
  id?: string;
  children?: any;
  color?: 'green' | 'gray' | 'dark-gray' | 'transparent';
  shape?: 'pill' | 'block';
  effect?: 'gleam' | 'fog';
  disabled?: boolean;
  hoverEffect?: 'scale';
  activeEffect?: 'translate';
  tooltip?: string;
  callback: () => void;
}

const Button: Component<ButtonProps> = (props) => {
  const {
    id,
    children,
    color = 'green',
    shape = 'pill',
    effect,
    disabled = false,
    hoverEffect,
    activeEffect,
    tooltip,
    callback
  } = props;

  const hoverClass = hoverEffect ? `hover-${hoverEffect}` : '';
  const activeClass = activeEffect ? `active-${activeEffect}` : '';
  const disabledClass = disabled ? 'pointer-events-none' : '';

  const buttonClassName = `${shape}-button button-${color} ${hoverClass} ${activeClass} ${disabledClass}`.trim();

  return (
    <button
      id={id}
      class={buttonClassName}
      data-tooltip={tooltip}
      disabled={disabled}
      onMouseUp={callback}
    >
      <span class="relative z-10">{children}</span>
      {effect && <span class={`${effect}-effect pointer-events-none`} />}
    </button>
  );
};

export default Button;
