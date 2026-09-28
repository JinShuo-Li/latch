def schedule(tasks, runner):
    if any(dependency not in tasks for dependencies in tasks.values() for dependency in dependencies):
        raise ValueError("unknown dependency")
    result = []
    state = {}

    def visit(name):
        if state.get(name) == 1:
            raise ValueError("dependency cycle")
        if state.get(name) == 2:
            return
        state[name] = 1
        for dependency in tasks[name]:
            visit(dependency)
        state[name] = 2
        result.append(name)

    for name in tasks:
        visit(name)
    for name in result:
        runner(name)
    return result
