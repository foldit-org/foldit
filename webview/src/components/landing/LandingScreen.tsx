import wiggle_animation from '../../assets/wiggle_animation.gif'

import { useUI } from '../../hooks/state';
import { openSessionDialog } from '../../transport'

import { LandingHeader, LandingCredits } from './LandingScreenAssets'
import NewsWidget from './NewsWidget'
import Button from '../util/Button'

export default function LandingScreen() {
	const { toggleWidget } = useUI();

	const playClick = () => { toggleWidget('puzzleMenu') }
	const optionsClick = () => { console.log('options button') }

	// Note: visibility is now controlled by parent (App.tsx) based on appScreen

	return (
		<div class="pointer-events-auto relative flex flex-col w-full h-full bg-gray-900 justify-between overflow-hidden">
			<div class="flex flex-row items-start justify-between h-[50vh]">
				<LandingHeader />
				<NewsWidget />
			</div>
			<div class="flex flex-row items-end justify-between h-[50vh] p-4">
				<div class="flex flex-col items-start ml-[10%] mb-10 space-y-5">
					<Button color="green" hoverEffect="scale" activeEffect="translate" effect="gleam" callback={playClick}>
						<div class="p-2 text-[18px] w-[150px]">PLAY FOLDIT</div>
					</Button>
					<Button color="green" effect="gleam" callback={() => openSessionDialog()}>
						<div class="py-1 w-[150px] text-[16px] whitespace-nowrap text-center">LOAD SESSION</div>
					</Button>
					<Button color="gray" disabled={true} effect="gleam" callback={optionsClick}>
						<div class="py-1 w-[100px] text-[16px]">OPTIONS</div>
					</Button>
				</div>
				<LandingCredits />
			</div>

			{/* This is the background animation of the protein wiggling */}
			<span class="absolute flex flex-col inset-0 justify-center opacity-20 blur-[10px] pointer-events-none">
				<img class="w-[1000px]" src={wiggle_animation} />
			</span>
		</div>
	);
}
