import { userSchema } from '$lib/schemas';
import { z } from 'zod';
import { backendUrl } from '$lib/data';

export const ssr = false;

export async function load({ fetch }) {
	const req = fetch(backendUrl + '/users/@me', {
		credentials: "include",
		mode: "same-origin"
	});
	req.catch(console.error);
	const response = await req;
	let user: z.infer<typeof userSchema> | null = null;
	if (response.ok) {
		const data = await response.json()
		console.log(data)
		user = userSchema.parse(data);
	}
	return { user };
}