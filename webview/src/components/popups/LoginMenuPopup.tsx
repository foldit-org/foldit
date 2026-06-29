import { createSignal, Show } from 'solid-js'

// component imports
import Button from '../util/Button'

// state imports
import { useUI } from '../../services/adapters'

// style imports
import '../../styles/popups/PopupMenu.css'

export default function LoginMenuPopup() {
	const [username, setUsername] = createSignal<string>('')
	const [password, setPassword] = createSignal<string>('')
	const { loginMenu, toggleWidget } = useUI();

	const loginCallback = (e: Event) => {
		console.log((e.currentTarget as HTMLElement).textContent);
		toggleWidget('loginMenu');
	}

	return (
		<Show when={loginMenu()}>
			<div class="foldit-popup-menu">
				<div class="foldit-popup-menu-content w-[40%] h-[30%]">
					<button
						class="exit" onClick={loginCallback}>
						✕
					</button>
					<h3>Login</h3>
					<div class="menu-body">
						<form onSubmit={loginCallback}>
							<div class="form-item">
								<label for="username">Username</label>
								<input
									id="username" type="username" value={username()}
									autocomplete='username' required
									onInput={e => setUsername((e.target as HTMLInputElement).value)}
								/>
							</div>
							<div class="form-item">
								<label for="password">Password</label>
								<input
									id="password" type="password" value={password()}
									autocomplete='current-password' required
									onInput={e => setPassword((e.target as HTMLInputElement).value)}
								/>
							</div>
							<div class="form-item">
								<Button
									callback={() => console.log('calling pillbutton callback')}
								>
									Login
								</Button>
							</div>
						</form>
					</div>
				</div>
			</div>
		</Show>
	)
}
