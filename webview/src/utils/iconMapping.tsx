/**
 * Icon mapping utility to replace lucide-react icons with solid-icons equivalents
 */

import type { JSX } from 'solid-js';
import { pluginAssetUrl, gameAssetUrl } from '../pluginAssets';
import {
  TbArrowLeft, TbArrowRight, TbRefresh, TbCheck, TbRefreshAlert,
  TbList, TbX, TbHelp, TbRotate, TbEye, TbSettings, TbInfoCircle,
  TbDeviceFloppy, TbLifebuoy, TbActivity, TbMinus, TbSquare, TbLogout,
  TbChevronDown, TbStar, TbTrophy, TbTrash, TbReplace, TbDownload, TbCloud,
  TbAtom2
} from 'solid-icons/tb';

// Icon size and styling props interface for consistency
export interface IconProps {
  size?: number | string;
  color?: string;
  class?: string;
  style?: Record<string, any>;
}

// Default icon props
const defaultIconProps: IconProps = {
  size: 16,
  color: 'currentColor'
};

// Icon mapping from lucide-react to solid-icons (Tabler icons)
export const Icons = {
  ArrowLeft: (props: IconProps = {}) => <TbArrowLeft {...defaultIconProps} {...props} />,
  ArrowRight: (props: IconProps = {}) => <TbArrowRight {...defaultIconProps} {...props} />,
  RefreshCcw: (props: IconProps = {}) => <TbRefresh {...defaultIconProps} {...props} />,
  RefreshCw: (props: IconProps = {}) => <TbRefreshAlert {...defaultIconProps} {...props} />,
  Check: (props: IconProps = {}) => <TbCheck {...defaultIconProps} {...props} />,
  List: (props: IconProps = {}) => <TbList {...defaultIconProps} {...props} />,
  X: (props: IconProps = {}) => <TbX {...defaultIconProps} {...props} />,
  HelpCircle: (props: IconProps = {}) => <TbHelp {...defaultIconProps} {...props} />,
  RotateCcw: (props: IconProps = {}) => <TbRotate {...defaultIconProps} {...props} />,
  Eye: (props: IconProps = {}) => <TbEye {...defaultIconProps} {...props} />,
  Settings: (props: IconProps = {}) => <TbSettings {...defaultIconProps} {...props} />,
  Info: (props: IconProps = {}) => <TbInfoCircle {...defaultIconProps} {...props} />,
  Save: (props: IconProps = {}) => <TbDeviceFloppy {...defaultIconProps} {...props} />,
  LifeBuoy: (props: IconProps = {}) => <TbLifebuoy {...defaultIconProps} {...props} />,
  Activity: (props: IconProps = {}) => <TbActivity {...defaultIconProps} {...props} />,
  Minimize: (props: IconProps = {}) => <TbMinus {...defaultIconProps} {...props} />,
  Maximize: (props: IconProps = {}) => <TbSquare {...defaultIconProps} {...props} />,
  LogOut: (props: IconProps = {}) => <TbLogout {...defaultIconProps} {...props} />,
  ChevronDown: (props: IconProps = {}) => <TbChevronDown {...defaultIconProps} {...props} />,
  Star: (props: IconProps = {}) => <TbStar {...defaultIconProps} {...props} />,
  Trophy: (props: IconProps = {}) => <TbTrophy {...defaultIconProps} {...props} />,
  Trash: (props: IconProps = {}) => <TbTrash {...defaultIconProps} {...props} />,
  Replace: (props: IconProps = {}) => <TbReplace {...defaultIconProps} {...props} />,
  Download: (props: IconProps = {}) => <TbDownload {...defaultIconProps} {...props} />,
  Cloud: (props: IconProps = {}) => <TbCloud {...defaultIconProps} {...props} />,
  Atom: (props: IconProps = {}) => <TbAtom2 {...defaultIconProps} {...props} />,
};

// Built-in GUI icons addressable by name from the backend. An ActionInfo /
// PanelInfo `icon_path` of the form `builtin:<name>` resolves here instead of
// fetching a plugin asset, letting native actions ship a glyph with no asset
// file. Keys are the `<name>` after the `builtin:` prefix.
const BUILTIN_ACTION_ICONS: Record<string, (p?: IconProps) => JSX.Element> = {
  replace: Icons.Replace,
  download: Icons.Download,
  cloud: Icons.Cloud,
  atom: Icons.Atom,
};

export function builtinActionIcon(name: string): ((p?: IconProps) => JSX.Element) | undefined {
  return BUILTIN_ACTION_ICONS[name];
}

/** A resolved icon token: an inline component, a fetchable URL, or nothing. */
export type ResolvedIcon =
  | { kind: 'component'; Icon: (p?: IconProps) => JSX.Element }
  | { kind: 'url'; url: string }
  | { kind: 'none' };

/**
 * Resolve any backend icon token, for buttons and picker options alike.
 *
 *   builtin:<name>            -> inline GUI glyph, no asset fetch
 *   game:<path>               -> foldit-owned asset under /game-assets/
 *   <path>                    -> plugin-relative asset under /plugins/<id>/
 *
 * `builtin:` is checked first so a glyph never resolves to an asset URL. An
 * unknown builtin name resolves to `none` rather than a broken image.
 */
export function resolveIcon(
  token: string | null | undefined,
  pluginId: string,
): ResolvedIcon {
  if (!token) return { kind: 'none' };
  if (token.startsWith('builtin:')) {
    const Icon = builtinActionIcon(token.slice('builtin:'.length));
    return Icon ? { kind: 'component', Icon } : { kind: 'none' };
  }
  if (token.startsWith('game:')) {
    return { kind: 'url', url: gameAssetUrl(token.slice('game:'.length)) };
  }
  return { kind: 'url', url: pluginAssetUrl(pluginId, token) };
}

// For backward compatibility, export individual icons
export const {
  ArrowLeft,
  ArrowRight,
  RefreshCw,
  List,
  X,
  HelpCircle,
  RotateCcw,
  Eye,
  Settings,
  Info,
  Save,
  LifeBuoy,
  Activity,
  Minimize,
  Maximize,
  LogOut,
  ChevronDown,
  Star,
  Trophy
} = Icons;