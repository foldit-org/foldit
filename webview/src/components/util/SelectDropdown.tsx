/**
 * SelectDropdown Components
 *
 * SolidJS select dropdown components.
 */

import { Component, For } from 'solid-js';
import { ChevronDown } from '../../utils/iconMapping';
import '../../styles/util/SelectDropdown.css';

export interface SelectDropdownProps {
  title: string;
  options: string[];
  value: string;
  option_name: string;
  onChange: (option_name: string, newValue: string) => void;
}

const SelectDropdown: Component<SelectDropdownProps> = (props) => {
  const handleChange = (event: Event) => {
    const target = event.target as HTMLSelectElement;
    props.onChange(props.option_name, target.value);
  };

  return (
    <div class="options-dropdown">
      <label>{props.title}</label>
      <select value={props.value} onChange={handleChange}>
        <For each={props.options}>
          {(option) => (
            <option value={option}>{option}</option>
          )}
        </For>
      </select>
      <div class="pointer-events-none absolute right-3 top-[36px] text-gray-400">
        <ChevronDown size={16} />
      </div>
    </div>
  );
};

export default SelectDropdown;
