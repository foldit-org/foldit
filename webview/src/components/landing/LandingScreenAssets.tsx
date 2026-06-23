import logo_name from '../../assets/logo_name.svg';

const text_gradient = 'bg-gradient-to-b from-white from-[30%] to-slate-500 bg-clip-text text-transparent';

export function LandingHeader() {
	const wide_column = 'flex flex-col relative w-[400px] h-[200px]';

	return (
		<span class={`${wide_column} mt-[20vh] ml-[10vw]`}>
			<span class="relative z-10 w-[300px]">
				<img src={logo_name} alt="logo" />
			</span>
			<span class={`relative z-10 ml-[50px] ${text_gradient} text-2xl`}
				style={{ fontFamily: "'Varela Round', sans-serif" }}>
				Solve Puzzles for Science
			</span>
			<span class="absolute inset-0 bg-gray-700 blur-[30px] rounded-full opacity-40 z-0">
			</span>
		</span>
	);
}

export function LandingCredits() {
	return (
		<span class="inline-block align-bottom text-right text-white text-sm text-opacity-70">
			<h3>DEVELOPED BY:</h3>
			<ul>
				<li>UNIVERSITY OF WASHINGTON</li>
				<li>UNIVERSITY OF CALIFORNIA</li>
				<li>UNIVERSITY OF DENVER</li>
				<li>UNIVERSITY OF MASSACHUSETTS</li>
				<li>NORTHEASTERN UNIVERSITY</li>
				<li>VANDERBILT UNIVERSITY</li>
			</ul>
		</span>
	);
}

