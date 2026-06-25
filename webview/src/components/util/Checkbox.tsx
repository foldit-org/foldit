/**
 * Checkbox Components
 *
 * SolidJS checkbox components.
 */

import { Component } from 'solid-js';
import '../../styles/util/Checkbox.css';

export interface CheckboxProps {
  label: string;
  checked: boolean;
  option_name: string;
  onToggle: (option_name: string, checked: boolean) => void;
}

const Checkbox: Component<CheckboxProps> = (props) => {
  const handleChange = (event: Event) => {
    const target = event.target as HTMLInputElement;
    props.onToggle(props.option_name, target.checked);
  };

  return (
    <label class="options-checkbox">
      <input
        type="checkbox"
        checked={props.checked}
        onChange={handleChange}
      />
      <span class="text-white text-[14px]">{props.label}</span>
    </label>
  );
};

export default Checkbox;
