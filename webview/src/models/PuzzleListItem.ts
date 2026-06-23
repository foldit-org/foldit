interface PuzzleListItem {
	id: number
	title: string
	description: string
	categoryId?: number | null
}

export class Category implements PuzzleListItem {
	id: number
	title: string
	description: string
	constructor(id: number, title: string, description: string) {
		this.id = id
		this.title = title
		this.description = description
	}
}

export class Puzzle implements PuzzleListItem {
	id: number
	title: string
	description: string
	categoryId: number
	constructor(id: number, title: string, description: string, categoryId: number) {
		this.id = id
		this.title = title
		this.description = description
		this.categoryId = categoryId
	}
}

