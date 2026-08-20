def saludar(nombre, formal=False):
    """Devuelve un saludo. En modo formal usa usted."""
    if formal:
        return f"Buenos días, {nombre}. ¿Cómo está usted?"
    return f"¡Hola, {nombre}!"
