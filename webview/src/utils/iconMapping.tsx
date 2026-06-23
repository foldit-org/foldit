/**
 * Icon mapping utility to replace lucide-react icons with solid-icons equivalents
 */

import { 
  TbArrowLeft, TbArrowRight, TbRefresh, TbCheck, TbRefreshAlert, 
  TbList, TbX, TbHelp, TbRotate, TbEye, TbSettings, TbInfoCircle,
  TbDeviceFloppy, TbLifebuoy, TbActivity, TbMinus, TbSquare, TbLogout,
  TbChevronDown, TbStar, TbTrophy, TbTrash 
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
};

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