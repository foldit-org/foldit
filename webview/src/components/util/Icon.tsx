/**
 * Wrapper around solid-icons providing a simple interface for rendering
 * icons by name string.
 * 
 * Usage:
 *   <Icon name="x" size={24} />
 *   <Icon name="arrow-left" size={16} class="text-gray-500" />
 */

import { Component } from 'solid-js';
import { Icons, IconProps } from '../../utils/iconMapping';

// Map of kebab-case names to icon functions
const iconMap: Record<string, (props: IconProps) => any> = {
  'activity': Icons.Activity,
  'arrow-left': Icons.ArrowLeft,
  'arrow-right': Icons.ArrowRight,
  'check': Icons.Check,
  'chevron-down': Icons.ChevronDown,
  'eye': Icons.Eye,
  'help-circle': Icons.HelpCircle,
  'info': Icons.Info,
  'life-buoy': Icons.LifeBuoy,
  'list': Icons.List,
  'log-out': Icons.LogOut,
  'maximize': Icons.Maximize,
  'minimize': Icons.Minimize,
  'refresh-ccw': Icons.RefreshCcw,
  'refresh-cw': Icons.RefreshCw,
  'rotate-ccw': Icons.RotateCcw,
  'save': Icons.Save,
  'settings': Icons.Settings,
  'x': Icons.X,
};

export type IconComponentProps = {
  name: string;
  size?: number;
  class?: string;
  color?: string;
};

export const Icon: Component<IconComponentProps> = (props) => {
  const { name, size = 24, class: className, color, ...rest } = props;

  const iconFunction = iconMap[name];
  if (!iconFunction) {
    console.warn(`Icon "${name}" not found in icon map`);
    return null;
  }

  return iconFunction({
    size,
    color,
    class: className,
    ...rest
  });
};

// Re-export for direct static usage (better tree-shaking)
export * from '../../utils/iconMapping';
