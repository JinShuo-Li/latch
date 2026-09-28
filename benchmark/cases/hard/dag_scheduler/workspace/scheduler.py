"""Task graph execution; tasks maps names to dependency lists."""


def schedule(tasks, runner):
    completed = []
    for name, dependencies in tasks.items():
        runner(name)
        completed.append(name)
        for dependency in dependencies:
            if dependency not in completed:
                runner(dependency)
                completed.append(dependency)
    return completed
