# Servidor FHIR Helios — catálogo de mensajes de la interfaz
# Configuración regional: Español (es)
#
# Mantenga las mismas claves que en `en/main.ftl` (la configuración regional
# de origen). Las claves que falten recurren a inglés según la cadena de
# reserva descrita en docs/multi-language.md.

## Marca / términos compartidos

-app-name = Servidor FHIR Helios
-org-name = Helios Software

## Estructura de página

app-title = { -app-name }
app-tagline = Un servidor FHIR rápido y multiversión

nav-dashboard = Panel
nav-terminology = Terminología
nav-terminology-new-window = Terminología (se abre en una pestaña nueva)
nav-resources = Recursos
nav-settings = Configuración
nav-signout = Cerrar sesión

## Selector de idioma

language-label = Idioma
language-en = Inglés
language-es = Español
language-de = Alemán
language-zh-hant = Chino tradicional
user-menu-label = Menú de la cuenta
user-anonymous = Usuario anónimo
user-local-hint = La autenticación está deshabilitada
user-logout = Cerrar sesión

## Página de inicio

home-lede = Interfaz renderizada en el servidor y basada en HTMX. Este panel se actualiza como un fragmento HTML.

## Panel de estado

status-last-checked = Última comprobación: { $timestamp }

## Panel / estado

dashboard-heading = Panel del servidor
health-status-ok = Todos los sistemas operativos
health-status-degraded = Algunos sistemas están degradados
health-uptime = Tiempo activo: { $duration }

resource-count = { $count ->
    [one] { $count } recurso
   *[other] { $count } recursos
}

## Exploración de terminología

terminology-heading = Servidor de terminología
terminology-lede = Conecta HFS con un servidor de terminología FHIR.
terminology-configured-heading = Servidor de terminología configurado
terminology-configured-body = HFS_TERMINOLOGY_SERVER apunta a una URL válida.
terminology-configured-open = Abrir servidor de terminología
terminology-invalid-heading = HFS_TERMINOLOGY_SERVER no es válida.
terminology-invalid-body = Usa una URL HTTP o HTTPS absoluta con un host. Se permiten rutas y una barra final. No incluyas credenciales, parámetros de consulta ni fragmentos.
terminology-invalid-note = Actualiza la variable de entorno y luego reinicia HFS.
terminology-setup-heading = Conectar un servidor de terminología
terminology-setup-body = Define HFS_TERMINOLOGY_SERVER con la URL base del servidor de terminología FHIR que debe usar HFS.
terminology-setup-note = Define la variable en el entorno desde el que se inicia HFS y luego reinicia el servidor.
terminology-search-label = Buscar CodeSystems y ValueSets
terminology-search-placeholder = p. ej. 73211009, «diabetes», http://snomed.info/sct
terminology-display-language = Idioma de visualización
terminology-no-results = No se encontraron conceptos coincidentes.

## Acciones comunes

action-search = Buscar
action-save = Guardar
action-cancel = Cancelar
action-retry = Reintentar

## Errores (refleja el texto de OperationOutcome; véase docs/multi-language.md §5)

error-not-found = No se encontró el recurso solicitado.
error-unauthorized = No está autorizado para realizar esta acción.
error-generic = Algo salió mal. Vuelva a intentarlo.

## Página genérica de "no encontrado" (#835): un 404 de página completa dentro
## del shell, distinta de error-not-found (texto de OperationOutcome para
## respuestas de la API).

not-found-title = No encontrado
not-found-text = Esta página no existe, o no está disponible para usted.

## Estructura del panel (Figma «Dashboard V1.1»)

nav-section-work = Trabajo
nav-section-batch-data = Lotes y datos
nav-section-sql-on-fhir = SQL on FHIR
nav-section-server = Servidor
nav-section-tools = Herramientas

nav-home = Inicio
nav-search = Buscar
nav-resource-editor = Editor de recursos
nav-history-versions = Historial y versiones
nav-compartments = Compartimentos
nav-batch-transaction = Lote / Transacción
nav-import = Importar
nav-export = Exportar
nav-sql-view-definitions = Definiciones de vistas
nav-sql-queries = Consultas SQL
nav-sql-views = Vistas SQL
nav-sql-export = Exportación SQL
nav-capability-conformance = Capacidad y conformidad
nav-search-parameters = Parámetros de búsqueda
nav-subscriptions = Suscripciones
nav-tenants = Tenants

## Mantenimiento de tenants (/ui/tenants)

tenants-title = Mantenimiento de tenants
tenants-lede = Aprovisiona, inspecciona y elimina los tenants entre los que este servidor aísla los datos.
tenants-unavailable = El registro de tenants no está disponible en este backend de almacenamiento.
tenants-stat-total = Tenants totales
tenants-stat-total-sub = { $count ->
    [one] { $count } registrado
   *[other] { $count } registrados
}
tenants-stat-resources = Recursos almacenados
tenants-stat-resources-sub = en todos los tenants
tenants-search-placeholder = Buscar por nombre o id de tenant…
tenants-add = Añadir tenant
tenants-add-title = Añadir un tenant
tenants-field-id = Id del tenant
tenants-field-id-hint = Se usa en la API (cabecera X-Tenant-ID, prefijo de URL, claim del JWT).
tenants-field-name = Nombre visible (opcional)
tenants-field-name-hint = Una etiqueta legible; no se usa para el enrutamiento.
tenants-add-submit = Aprovisionar tenant
tenants-col-tenant = Tenant
tenants-col-resources = Recursos
tenants-col-created = Creado
tenants-col-actions = Acciones
tenants-empty = Ningún tenant coincide.
tenants-unregistered = sin registrar
tenants-delete = Eliminar tenant
tenants-delete-confirm = ¿Dar de baja el tenant «{ $id }»? Sus datos se conservan salvo que se purguen vía API.
tenants-row-provisioning = Aprovisionando… puede tardar un momento.
tenants-row-failed = No se pudo aprovisionar el tenant.
tenants-dismiss = Descartar
# TODO review (machine translation)
tenants-error-id-empty = El id del tenant no puede estar vacío.
tenants-error-id-too-long = El id del tenant supera los { $max } caracteres.
tenants-error-id-reserved = Ese id está reservado para recursos compartidos internos.
tenants-error-id-invalid-character = El id del tenant solo puede contener letras, dígitos, '-', '_', '.' y '/'.
tenants-error-id-empty-segment = El id del tenant no puede empezar ni terminar con '/' ni contener un segmento vacío.
tenants-error-exists = El tenant '{ $id }' ya existe.
tenants-error-provisioning = El tenant '{ $id }' ya se está aprovisionando.
tenants-error-still-provisioning = El tenant '{ $id }' todavía se está aprovisionando.
tenants-error-not-purged = El tenant '{ $id }' se dio de baja, pero sus datos NO se purgaron: { $detail }
tenants-error-list = No se pudieron listar los tenants: { $detail }
tenants-error-count = No se pudieron contar los recursos: { $detail }
tenants-field-name-placeholder = Clínica Ejemplo

tenant-heading = Tenants
tenant-all = Todos los tenants
tenant-search-placeholder = Buscar tenants

theme-toggle-system = Tema: seguir al sistema
theme-toggle-light = Tema: claro
theme-toggle-dark = Tema: oscuro

fhir-version = FHIR { $version }
fhir-version-heading = Versión FHIR

card-resource-types = Tipos de recursos
card-resource-types-sub = usados para { $version }
card-stored-resources = Recursos almacenados
card-stored-resources-sub = en el tenant activo
card-export-jobs = Trabajos de exportación
card-export-jobs-sub = en ejecución ({ $queued } en cola)
card-import-jobs = Trabajos de importación
card-import-jobs-sub = activos
card-jobs-unavailable = no disponible
card-uptime = Tiempo activo
card-uptime-sub = desde el arranque del proceso

chart-title = Recursos FHIR en el tiempo
chart-window = Intervalo de tiempo del gráfico
chart-pick-heading = Tipos de recurso graficados
chart-pick-all = Ver todos los tipos de recurso
chart-pick-filter = Filtrar tipos
chart-empty = Nada que graficar todavía: los recursos almacenados aparecerán aquí a medida que se creen.
# Solo para una build sin proveedor de métricas. Un proveedor que simplemente
# tardó usa chart-pending-note: no deben confundirse nunca (#956).
chart-sample-note = Datos de muestra: esta build no tiene proveedor de métricas en vivo, así que el gráfico de abajo es un marcador de posición, no mediciones.
chart-pending-note = Todavía se están recopilando las cifras en vivo de este intervalo. No se grafica nada hasta que lleguen: no se muestran números de relleno.
chart-pending-empty = Esperando las cifras en vivo…
chart-pending-retry = Reintentar ahora
# #1078: cifras medidas, pero contadas a partir de contadores de escritura en
# memoria, no de una lectura exacta del almacenamiento. Etiqueta, no aviso.
chart-approximate-note = Aproximado: contado a partir de las escrituras recientes y aún en conciliación con el almacenamiento.
chart-counts-unsupported = Los recuentos de recursos no están disponibles para este backend de almacenamiento.
chart-counts-unsupported-note = Este backend de almacenamiento no puede contar los recursos almacenados, por lo que no se muestran totales ni gráfico. Esto no significa que el tenant esté vacío.
# #1078: cuándo se leyeron las cifras de la página. $time es una hora UTC,
# precedida de la fecha cuando no es de hoy (p. ej. "14:02:31 UTC").
chart-as-of = Última lectura: { $time }.
search-index-rebuilding = Reconstruyendo el índice de búsqueda: { $percent } % ({ $processed } de { $total } recursos). Las búsquedas pueden omitir recursos almacenados hasta que termine.
search-index-rebuilding-counting = Reconstruyendo el índice de búsqueda. Las búsquedas pueden omitir recursos almacenados hasta que termine.
search-index-rebuild-failed = { $errors ->
    [one] La última reconstrucción del índice de búsqueda dejó un recurso sin indexar. Las búsquedas no lo encuentran hasta que una reconstrucción termine bien; GET $reindex-status/{ $job } indica cuál.
   *[other] La última reconstrucción del índice de búsqueda dejó { $count } recursos sin indexar. Las búsquedas no los encuentran hasta que una reconstrucción termine bien; GET $reindex-status/{ $job } indica cuáles.
  }
search-index-rebuild-failed-job = La última reconstrucción del índice de búsqueda falló antes de terminar. Las búsquedas pueden omitir recursos almacenados hasta que una reconstrucción termine bien; GET $reindex-status/{ $job } indica el motivo.
chart-table-toggle = Ver como tabla
chart-table-when = Momento
chart-focus-series = Enfocar esta serie
chart-unfocus-series = Mostrar todas las series

## Pie de página

footer-copyright = © { $year } { -org-name }

## Historial y versiones (#236)

history-heading = Historial y versiones
history-lede = Compara dos versiones de un recurso. El almacenamiento está totalmente versionado; esto lo lee con la API estándar _history y vread.
history-type-label = Tipo de recurso
history-id-label = Id del recurso
history-id-placeholder = id del recurso
history-load = Cargar
history-tabs-label = Alcance del historial
history-tab-instance = Instancia
history-tab-type = Feed por tipo
history-tab-system = Feed del sistema
history-versions-label = Versiones
history-current = actual
history-from = Desde
history-to = Hasta
history-show-metadata = Mostrar cambios de metadatos
history-empty = Carga un recurso y elige dos versiones para comparar.
history-load-error = No se pudo cargar el historial de ese recurso.
history-not-found = No hay historial para ese recurso — revisa el tipo y el id.
history-diff-heading = { $from }
history-metadata-hidden = { $count ->
    [one] { $count } cambio de metadatos oculto
   *[other] { $count } cambios de metadatos ocultos
}
history-textual = Ver diff de texto completo
history-only-metadata = Entre estas versiones solo cambiaron los metadatos.
history-identical = Estas dos versiones son idénticas.
history-deleted = { $version } es una eliminación — no hay contra qué comparar.
history-parse-error = No se pudieron leer esas versiones como JSON.
## Saved queries (#234)

nav-saved-queries = Consultas guardadas

queries-heading = Consultas guardadas
queries-lede = Guarda consultas de búsqueda FHIR por tipo de recurso, ordenadas por su última ejecución. Se guardan en tu configuración de usuario y te siguen entre dispositivos.
queries-add-heading = Guardar una consulta
queries-type-label = Tipo de recurso
queries-type-placeholder = p. ej. Patient
queries-name-label = Nombre
queries-name-placeholder = p. ej. Smith en Boston
queries-query-label = Cadena de consulta
queries-query-placeholder = p. ej. name=smith&address-city=Boston
queries-empty = Aún no hay consultas guardadas. Guarda una arriba para empezar.
queries-never-run = Nunca ejecutada
queries-run = Ejecutar
queries-rename = Renombrar
queries-delete = Eliminar
queries-rename-prompt = Nuevo nombre
queries-confirm-delete = ¿Eliminar «{ $name }»?
queries-unavailable = Las consultas guardadas no están disponibles: el backend de almacenamiento de este servidor no admite configuración por usuario.

## Visor de SearchParameters (#238)

sp-heading = Parámetros de búsqueda
sp-lede = Explora los parámetros con los que este servidor resuelve las búsquedas, filtrados por tipo de recurso base. Los parámetros almacenados se pueden crear, editar y eliminar; el registro recoge los cambios por tenant.
sp-version-label = Versión FHIR
sp-degraded = Los parámetros de búsqueda no se pudieron cargar de este servidor en este momento — la auto-llamada a /SearchParameter falló (con autenticación habilitada esto suele significar que el token de servicio saliente falta o es inválido). La página reintenta en la siguiente petición.
sp-rail-label = Filtro de recursos
sp-rail-search = Filtrar tipos
sp-rail-recent = Usados recientemente
sp-rail-types = Tipos de recurso
sp-rail-all = Todos los tipos
sp-facet-type = Tipo
sp-facet-type-label = Filtrar por tipo de parámetro
sp-facet-source = Origen
sp-facet-source-label = Filtrar por origen
sp-source-embedded = embebido
sp-source-stored = almacenado
sp-source-config = configuración
sp-chip-conflict = conflicto
sp-chip-overrides = anula la spec
sp-chip-shadowed = eclipsado
sp-col-code = Código
sp-col-type = Tipo
sp-col-base = Base
sp-col-expression = Expresión
sp-col-source = Origen
sp-total = { $count } parámetros
sp-pagination-label = Páginas
sp-page-prev = Anterior
sp-page-next = Siguiente
sp-detail-label = Detalle del parámetro
sp-detail-empty = Ningún parámetro seleccionado
sp-detail-empty-hint = Selecciona una fila para inspeccionar su definición, su expresión y cómo se resuelve en el registro.
sp-detail-readonly = Parámetro de la especificación (compilado desde el archivo de datos) — solo lectura.
sp-field-url = URL canónica
sp-field-name = Nombre
sp-field-status = Estado
sp-field-base = Tipos de recurso base
sp-field-expression = Expresión FHIRPath
sp-field-description = Descripción
sp-field-target = Tipos destino
sp-field-components = Componentes
sp-status-hint = El cargador promueve el estado draft de la especificación a active al cargar.
sp-note-conflict = (base, code) duplicado dentro del mismo origen que { $url } — el registro rechaza esta colisión (DuplicateCode).
sp-note-overrides = Anula a { $url } en (base, code): una definición almacenada tiene precedencia sobre el parámetro de la spec, así que esta resuelve las búsquedas. El registro emite un WARN con ambas URLs.
sp-note-shadowed = Eclipsado por { $url } en (base, code): un origen de mayor precedencia resuelve las búsquedas de este slot.
sp-note-empty-expression = Expresión vacía: el extractor no indexa ninguna fila, así que toda búsqueda con este parámetro devuelve vacío en silencio.
sp-note-no-target = Parámetro de referencia sin tipos destino: la búsqueda encadenada no puede resolver el tipo referenciado.
sp-note-choice-type = Expresión de tipo choice: el extractor reescribe ofType(T) / as T al elemento concreto (por ejemplo valueQuantity) antes de evaluar contra el JSON almacenado.
sp-new = Nuevo parámetro de búsqueda
sp-edit = Editar
sp-delete = Eliminar
sp-delete-confirm = ¿Eliminar este parámetro de búsqueda almacenado? Las búsquedas que lo usan dejarán de coincidir cuando el registro se actualice.
cmp-new = Nueva definición de compartimento
cmp-edit = Editar
cmp-delete = Eliminar
cmp-delete-confirm = ¿Eliminar esta definición de compartimento? Sus rutas de compartimento dejarán de resolverse.
crud-delete-failed = No se pudo eliminar el elemento.

## Visor y probador de compartments (#237)

cmp-heading = Compartimentos
cmp-lede = Las definiciones de compartment con las que este servidor enruta las peticiones /{"{"}compartment{"}"}/{"{"}id{"}"}/{"{"}type{"}"}, y un probador que responde: ¿está este tipo en este compartment, mediante qué parámetros, y qué búsqueda ejecuta el servidor?
cmp-rail-label = Definiciones de compartment
cmp-rail-heading = Compartimentos
cmp-degraded = Las definiciones de compartimento no se pudieron cargar de este servidor en este momento — la auto-llamada a /CompartmentDefinition falló (con autenticación habilitada esto suele significar que el token de servicio saliente falta o es inválido). La página reintenta en la siguiente petición.
cmp-rail-note = Las definiciones son recursos almacenados, sembrados desde la especificación FHIR al arrancar. Las ediciones y eliminaciones aquí son por tenant.
cmp-tabs-label = Secciones del compartment
cmp-tab-definition = Definición
cmp-tab-members = Miembros
cmp-tab-tester = Probador
cmp-field-code = Código
cmp-field-status = Estado
cmp-field-url = URL canónica
cmp-field-version = Versión
cmp-field-publisher = Editor
cmp-field-description = Descripción
cmp-field-search = search
cmp-field-experimental = experimental
cmp-search-why = Apagado significaría que ninguna ruta de compartment resuelve para este compartment.
cmp-on = activado
cmp-off = desactivado
cmp-yes = sí
cmp-no = no
cmp-readonly-note = Solo lectura: estos valores provienen de las definiciones de la especificación compiladas en el servidor.
cmp-filter-members = Miembros
cmp-filter-all = Todos los tipos
cmp-filter-excluded = Excluidos
cmp-member = miembro
cmp-excluded = excluido
cmp-tester-id = Id
cmp-tester-target = Tipo destino (o *)
cmp-tester-run = Probar
cmp-result-member = ✓ miembro — vía { $params }
cmp-result-flat = // búsqueda plana equivalente
cmp-result-member-note = El servidor resuelve la ruta de compartment a esta búsqueda sobre los parámetros de referencia del tipo.
cmp-result-self = ✓ miembro — el propio recurso del compartment ({"{"}def{"}"})
cmp-result-self-note = La instancia del compartment está trivialmente en su propio compartment; la ruta lee el recurso directamente.
cmp-result-notmember = ✕ { $type } no es miembro de este compartment
cmp-result-notmember-note = El servidor devuelve 404 con un OperationOutcome para tipos que no son miembros del compartment.
cmp-result-fanout = Se expande a { $count } tipos miembro
cmp-result-fanout-note = Los tipos excluidos se omiten, no fallan — el fan-out descarta los tipos no miembro en lugar de dar error.
queries-builder-heading = Constructor de búsquedas
queries-url-label = URL de búsqueda FHIR
queries-url-placeholder = GET /Patient?name=smith&birthdate=ge1980-01-01
queries-builder-hint = Edita la URL GET directamente o mediante las filas de abajo — se mantienen sincronizadas. Ejecutar corre la búsqueda aquí mismo y la registra en Recientes; ponle un nombre para conservarla en la lista.
queries-recent = Recientes
queries-recent-heading = Búsquedas recientes
queries-recent-empty = Aún no hay búsquedas recientes — ejecuta una para registrarla aquí.
queries-invalid-url = Escribe una búsqueda como GET /Patient?name=smith — el tipo de recurso sale de la ruta.
queries-invalid-fhir-escape = Esta consulta contiene un escape FHIR no válido. Corrige el valor escapado antes de editarlo visualmente.

queries-conditions = Condiciones
queries-add-condition = Añadir condición
queries-includes = Includes
queries-result-controls = Controles de resultado
queries-remove = Quitar
queries-match-is = es
queries-or = + o
plain-pill = En lenguaje claro
plain-find = Buscar registros de {"{type}"}
plain-clause = {"{path}"} {"{verb}"} {"{value}"}
plain-clause-no-value = {"{path}"} {"{verb}"}
plain-and = y
plain-or = o
plain-arrow = {" "}→
plain-has = que tienen un {"{type}"} relacionado cuyo {"{param}"} {"{verb}"} {"{value}"}
plain-has-no-value = que tienen un {"{type}"} relacionado cuyo {"{param}"} {"{verb}"}
plain-include = Devolviendo también el {"{param}"} de cada {"{type}"}{"{target}"}
plain-revinclude = Más cada {"{type}"} cuyo {"{param}"} apunta aquí
plain-iterate = (repetidamente)
plain-count = Mostrando {"{n}"} por página
plain-unknown-param = "{"{param}"}" no es un parámetro de búsqueda de {"{type}"}
plain-sort = Ordenado por {"{sort}"}
plain-verb-is = es
plain-verb-contains = contiene
plain-verb-exact = es exactamente
plain-verb-missing = está presente/ausente
plain-verb-missing-true = está ausente
plain-verb-missing-false = está presente
plain-verb-not = no es
plain-verb-text = coincide con el texto
plain-verb-in = está en el value set
plain-verb-not-in = no está en el value set
plain-verb-identifier = tiene el identificador
plain-verb-of-type = tiene un identificador de tipo
plain-verb-ge = es igual o posterior a
plain-verb-le = es igual o anterior a
plain-verb-gt = es posterior a
plain-verb-lt = es anterior a
plain-verb-ne = no es
plain-verb-eq = es
plain-verb-sa = comienza después de
plain-verb-eb = termina antes de
plain-verb-ap = es aproximadamente
queries-related-heading = Incluir datos relacionados
queries-related-sub = Añade recursos conectados a los resultados.
queries-related-add-include = Incluir un recurso al que apunta
queries-related-add-revinclude = Incluir recursos que apuntan aquí
queries-iterate = Iterar
queries-sort-label = Orden
queries-sort-default = Predeterminado
queries-sort-recent = Más recientes
queries-sort-oldest = Más antiguos
queries-sort-id = ID
queries-modify-heading = Modificadores
queries-mod-exact = valor completo, incl. mayúsculas y acentos
queries-mod-contains = coincide en cualquier parte del texto
queries-mod-missing = el campo está presente / ausente
queries-mod-text = tratamiento avanzado de texto
queries-mod-not = ningún valor coincide
queries-mod-above = este o un ancestro
queries-mod-below = este o un descendiente
queries-mod-in = miembro del value set
queries-mod-not-in = no es miembro del value set
queries-mod-identifier = compara la referencia por identificador
queries-mod-of-type = compara tipo, sistema y valor del identificador
queries-chain-into = Filtrar por una propiedad del recurso referenciado
queries-chain-any-target = cualquiera
queries-has-pill = tiene un recurso relacionado
queries-has-type-placeholder = tipo de recurso
queries-has-via = enlazado vía
queries-has-where = donde su
queries-add-has = ⧉ Filtrar un recurso que enlaza aquí
queries-param-placeholder = parámetro
queries-param-none = Ningún parámetro coincide
queries-param-unknown = No es un parámetro de búsqueda de {"{type}"}. Elige uno de la lista.
queries-value-placeholder = valor
queries-results = Resultados
queries-results-total = { $count } resultados
queries-results-total-partial = { $count }+ resultados
queries-results-included = { $count } incluidos
queries-results-empty = Sin resultados.
queries-searching = Buscando…
queries-search-elapsed = { $seconds } segundos transcurridos
queries-search-cancel = Cancelar
queries-search-slow = Esta búsqueda está tardando más de lo esperado. Puedes seguir esperando o cancelarla.
queries-search-keep-waiting = Seguir esperando
queries-results-previous = Resultados anteriores
queries-col-updated = Actualizado
queries-copy-id = Copiar id
queries-copied = Copiado
queries-prev = Anterior
queries-next = Siguiente
queries-results-fetch-error = No se pudieron cargar los resultados desde { $origin }. Revise HFS_BASE_URL e inténtelo de nuevo.

queries-rail-heading = Tipos de recurso
queries-rail-filter = Filtrar tipos

## Búsqueda — lenguaje natural y constructor visual (#255)

search-heading = Buscar
search-lede = Describe lo que buscas, o arma la consulta a mano. En ambos casos obtienes una búsqueda FHIR que puedes leer, corregir y ejecutar.
search-query-tag = CONSULTA
search-copy = Copiar la consulta

search-mode-label = Cómo escribir la consulta
search-mode-nl = Lenguaje natural
search-mode-builder = Constructor visual

search-nl-label = Describe la búsqueda
search-nl-placeholder = Describe lo que buscas — p. ej. pacientes de apellido Smith nacidos después de 1980
search-nl-hint = Tu texto y los parámetros de búsqueda de este servidor van al modelo de lenguaje. Los datos de pacientes nunca. La consulta que escribe se muestra abajo para que la revises y la ejecutes.
search-nl-working = Traduciendo…
search-nl-caveats = Ten en cuenta:
search-nl-unsupported = Eso no es una búsqueda que este servidor pueda ejecutar. Prueba describiendo los registros que quieres encontrar.

search-nl-example-1 = Pacientes mujeres mayores de 65 con diagnóstico de diabetes
search-nl-example-2 = Observaciones de los últimos 30 días, las más recientes primero
search-nl-example-3 = Encuentros en Boston General todavía en curso

search-setup-heading = La búsqueda en lenguaje natural está disponible
search-setup-body = Convierte descripciones en lenguaje llano en consultas de búsqueda FHIR. Necesita una clave de API de un modelo de lenguaje — el servidor la lee del entorno y nunca llega a esta página. Mientras no haya una, usa el constructor visual de abajo.
search-setup-key-placeholder = tu clave de API
search-setup-disable = Para eliminar la función por completo — endpoint, página y este aviso — define HFS_NL_SEARCH_ENABLED=false.
search-setup-docs = Leer el instructivo

## Editor de recursos (#264)

editor-heading = Editor de recursos
editor-lede = Edita un recurso contra su esquema: añade cualquier elemento que el esquema permita, a cualquier profundidad — incluidas extensiones, en cualquier nodo que las acepte.
editor-title = Editar recurso
editor-view-label = Cómo editar
editor-view-form = Formulario guiado
editor-view-json = JSON
editor-save = Guardar cambios
editor-delete = Eliminar
editor-remove = Quitar este nodo
editor-saved = Guardado.
# Shared unsaved-changes tracker (#1240): the pill next to a Save button and
# the confirm shown for in-page closes (a modal, an addbox disclosure).
unsaved-changes = Cambios sin guardar
unsaved-discard-confirm = Hay cambios sin guardar. ¿Descartarlos y cerrar?
editor-load-error = No se pudo cargar ese recurso.
editor-load-not-found = No existe ningún { $type } con id { $id } en este servidor — compruebe el id o vuelva a la lista.
editor-confirm-delete = ¿Eliminar este recurso? No se puede deshacer.
editor-invalid-json = Eso no es JSON válido, así que no puede editarse como formulario. Tu texto queda intacto.
editor-source-hint = Edita el código directamente. Al volver al formulario guiado se interpreta.

editor-add = Añadir elemento
editor-add-close = Cerrar
editor-add-added = añadido
editor-add-undo = Deshacer
editor-add-elements = Elementos
editor-add-extensions = Extensiones
editor-must-support-badge = MS
editor-binding-hint = Ligado a un value set — los códigos salen de él; se muestra la fuerza
editor-legend-live = Se comprueba al escribir: estructura, cardinalidad, bindings requeridos
editor-legend-save = Se comprueba al guardar: constraints y terminología
vd-form-legend-live = Se comprueba al escribir: estructura, cardinalidad, bindings requeridos y sintaxis FHIRPath / reglas SQL-on-FHIR
vd-form-invalid-chip = JSON no válido
lib-form-legend-live = Se comprueba al escribir: estructura, cardinalidad, bindings requeridos
lib-form-legend-save = Se comprueba al guardar: tipo de Library de SQL on FHIR y el adjunto SQL
lib-form-legend-content = El adjunto SQL (content) no aparece aquí: se edita en la tarjeta SQL de abajo
editor-deferred-badge = al guardar
editor-deferred-hint = Los códigos se verifican contra el value set al guardar (y en vivo en el picker si hay servidor de terminología configurado)
editor-must-support-hint = Must-support: se espera que los consumidores de este perfil manejen este elemento
editor-add-filter = Filtrar elementos y extensiones
editor-add-another = añadir otro
editor-pick-type = Elige un tipo…
editor-extension-url = URL de la extensión
editor-add-extension = Añadir extensión

editor-valid = Sin problemas.
editor-issues = { $count ->
    [one] { $count } problema
   *[other] { $count } problemas
}

editor-modifier-badge = modificadora
editor-modifier-warning = Una extensión modificadora cambia el significado del recurso. Un sistema que no la reconozca debe negarse a procesarlo.
editor-unknown-badge = fuera del esquema
editor-unknown-hint = El esquema no describe este elemento. Se muestra para que no se pierda en silencio, y se conserva al guardar.

editor-primitive-extension-badge = + extensión
editor-primitive-extension-hint = Este valor lleva extensiones propias (un hermano `_` en el JSON). Se conservan al guardar.

editor-collapse-all = Colapsar todo
editor-expand-all = Expandir todo
json-view-toggle-fold = Alternar sección JSON
editor-edit-raw = Editar crudo
editor-versions = Versiones
editor-versions-none = Sin versiones anteriores.
## Historial y versiones (#236)

## Espacio de recursos (#282)

resources-heading = Recursos
resources-lede = Explora, busca, crea y edita recursos FHIR. Busca en lenguaje natural o arma la consulta a mano, y abre cualquier resultado para editarlo.
resources-create = Crear recurso
resources-create-typed = Crear { $type }
resources-create-invalid-type = Este tipo de recurso no está disponible en la versión FHIR seleccionada. Corrige la consulta o elige un tipo de la lista.
resources-create-not-advertised = Este servidor no permite crear este tipo de recurso. Aún puedes buscarlo.
resources-create-schema-unavailable = Este tipo de recurso no tiene un esquema de edición en la versión FHIR seleccionada, por lo que la UI no puede crearlo de forma segura.
resources-create-metadata-unavailable = Las capacidades del servidor no están disponibles. La creación seguirá desactivada hasta que la UI pueda verificarlas.
resources-save-blocked = Corrige los problemas de validación antes de guardar.
resources-save-invalid = El JSON no es válido — corrígelo antes de guardar.
resources-edit-title = Editar recurso
resources-tab-edit = Editar
resources-tab-history = Historial
resources-types-heading = Tipos de recurso
rail-all-types-heading = Todos los tipos
rail-count-approximate = Aproximado: contado a partir de las escrituras recientes y aún en conciliación con el almacenamiento.

queries-saved-group = Guardadas

nav-collapse = Colapsar menú
nav-menu-open = Abrir el menú de navegación
nav-menu-close = Cerrar el menú de navegación
nav-main = Navegación principal

batch-heading = Batch / Transaction
batch-lede = Sube un Bundle FHIR, revisa las acciones que va a ejecutar, ejecútalo contra este servidor y lee el resultado de cada entrada.
batch-upload = Subir
batch-drop-hint = Suelta aquí un fichero JSON de bundle
batch-drop-browse = o haz clic para explorar
batch-invalid-json = Ese fichero no es JSON válido.
batch-not-a-bundle = Ese JSON no es un Bundle FHIR.
batch-bad-type = Aquí solo se ejecutan Bundles de tipo batch o transaction.
batch-request = Petición
batch-entries = entradas
batch-semantics-batch = Batch: las entradas se ejecutan de forma independiente — una entrada fallida no detiene ni deshace las demás.
batch-semantics-transaction = Transaction: todo o nada — si alguna entrada falla, el servidor revierte el bundle completo.
batch-tab-actions = Acciones
batch-tab-json = JSON del bundle
batch-no-body = (sin cuerpo — esta entrada solo direcciona un recurso)
batch-cancel = Cancelar
batch-execute = Ejecutar
batch-plan-heading = Plan de ejecución
batch-done = Hecho
batch-response-heading = Resultados por acción
batch-sum-created = creados
batch-sum-updated = actualizados
batch-sum-other = lecturas/otros
batch-sum-failed = fallidos
batch-request-failed = La petición falló.
batch-reading = Leyendo el bundle…
batch-executing = Ejecutando…
batch-read-failed = No se pudo leer el archivo.

## Bulk Import workspace (#527)

bulk-import-title = Importación masiva
bulk-import-lede = Envía conjuntos de datos FHIR precoordinados a un Data Recipient con la operación $bulk-submit de Bulk Data.
bulk-import-detail-lede = Los manifests, el estado y el registro de ejecución de este envío.
bulk-import-new = Nueva submission
bulk-import-create-title = Crear Bulk Submission
bulk-import-field-name = Nombre de la submission
bulk-import-auth = Autenticación
bulk-import-auth-none = Ninguna
bulk-import-auth-none-hint = No se enviará cabecera de autorización.
bulk-import-auth-backend = Autenticación backend services
bulk-import-auth-backend-hint = Obtiene un token de acceso y lo envía como Bearer en la cabecera de autorización.
bulk-import-field-client-id = Client ID
bulk-import-field-client-id-hint = Registre este proveedor de datos con el receptor y obtenga un client ID.
bulk-import-field-token-url = URL del token
bulk-import-field-token-url-hint = URL del endpoint de token del servidor de autorización.
bulk-import-jwks-hint = Registre la clave pública de este servidor con el destinatario mediante la URL de JWKS:
bulk-import-test-auth = Probar autenticación
bulk-import-test-auth-ok = Autenticación correcta.
bulk-import-create-submit = Enviar
bulk-import-advanced = Opciones avanzadas
bulk-import-unavailable = El backend de almacenamiento no aloja el settings store; no se pueden guardar submissions.
bulk-import-submissions = Submissions
bulk-import-records = registros
bulk-import-col-name = Nombre
bulk-import-col-status = Estado
bulk-import-col-created = Creada
bulk-import-col-destination = Destino
bulk-import-empty = Aún no hay submissions. Cree una para empezar.
bulk-import-all = Todas las submissions
bulk-import-status-not-started = Sin iniciar
bulk-import-status-in-progress = En curso
bulk-import-status-stopped = Detenida
bulk-import-status-completed = Completada
bulk-import-status-failed = Fallido
bulk-import-status-error = El último cambio de estado no llegó al receptor de datos, así que esta submission sigue igual: inténtelo de nuevo. ({ $detail })
bulk-import-status-pending = El último cambio de estado no llegó al receptor de datos; se conserva y se reenviará automáticamente mientras esta página siga abierta. ({ $detail })
bulk-import-detail-recipient = Receptor de datos
bulk-import-detail-id = ID de submission
bulk-import-detail-submitter = Remitente
bulk-import-detail-created = Creada
bulk-import-detail-status = Estado
bulk-import-detail-auth = Autenticación
bulk-import-abort = Abortar
bulk-import-mark-completed = Marcar completada
bulk-import-delete = Eliminar
bulk-import-edit = Editar
bulk-import-edit-title = Editar submission
bulk-import-edit-submit = Guardar cambios
bulk-import-field-manifest-url = URL del manifest
bulk-import-field-manifest-url-hint = URL de un Bulk Export Manifest con un conjunto de datos FHIR precoordinado.
bulk-import-field-output-format = Formato
bulk-import-field-output-format-hint = El formato de los archivos Bulk Data del manifest.
bulk-import-field-headers = Cabeceras de petición de archivos
bulk-import-field-headers-hint = Cabeceras HTTP que el receptor debe usar al pedir un archivo de datos, una "Nombre: valor" por línea.
bulk-import-log = Registro de la submission
bulk-import-log-empty = Todavía no se ha enviado nada.
bulk-import-field-submitter-system = Sistema del remitente
bulk-import-field-submitter-value = Valor del remitente
bulk-import-field-submitter-hint = Debe coincidir con un identificador registrado con el receptor (coordinado fuera de banda). Déjelo vacío para usar los valores generados.
bulk-import-processing = Procesando
bulk-import-processing-waiting = Esperando el primer reporte de estado del receptor…
bulk-import-result = Resultado
bulk-import-result-finished = Procesamiento terminado a las
bulk-import-result-outputs = Archivos de salida
bulk-import-result-errors = Archivos de error
ui-cancel = Cancelar
ui-close = Cerrar
ui-combobox-selected-label = Elementos seleccionados
ui-combobox-remove = Quitar
ui-combobox-added = Añadido
ui-combobox-removed = Quitado
ui-combobox-loading = Cargando sugerencias…
ui-combobox-results-updated = Sugerencias disponibles:
ui-combobox-error = No se pudieron cargar las sugerencias. Inténtalo de nuevo.
editor-orphans-title = Estos problemas aún no tienen campo — añada los elementos para corregirlos
editor-hint-date = FHIR date: YYYY, YYYY-MM o YYYY-MM-DD
editor-hint-datetime = FHIR dateTime: YYYY, YYYY-MM, YYYY-MM-DD o un timestamp completo con zona horaria (2024-05-17T14:30:00+02:00)
editor-hint-time = FHIR time: HH:MM:SS
editor-hint-instant = FHIR instant: timestamp completo con zona horaria, p. ej. 2024-05-17T14:30:00.000Z

## Página de suscripciones (#580)

subs-title = Suscripciones
subs-lede = Vista de solo lectura del motor de suscripciones: cada suscripción registrada, su canal, estado en vivo y contadores de entrega.
subs-unavailable = El motor de suscripciones no está habilitado en este servidor.
subs-unavailable-how = Actívalo arrancando HFS con:
subs-empty = No hay suscripciones registradas para este tenant.
subs-card-failing = Fallando
subs-card-failing-sub = Requiere atención
subs-card-idle = Inactivas
subs-card-idle-sub = Sin clientes
subs-card-active = Activas
subs-card-active-sub = entregando
subs-card-delivered = Entregadas en 24 h
subs-card-delivered-sub = { $rate }% al primer intento
subs-card-delivered-none = sin entregas en la ventana
subs-table-heading = Suscripciones
subs-sort = Ordenar
subs-sort-status = Estado
subs-sort-sent = Más enviadas
subs-sort-fails = Racha de fallos
subs-col-subscription = Suscripción
subs-col-channel = Canal
subs-col-status = Estado
subs-col-last24 = Últimas 24 h
subs-col-sent = Enviadas
subs-col-fails = Racha de fallos
subs-state-active = Activa
subs-state-error = Error
subs-state-idle = 0 clientes
subs-state-requested = Solicitada
subs-state-off = Apagada

## Bulk Export workspace (#537)

bulk-export-title = Exportación masiva
bulk-export-lede = Extrae datos de este servidor como archivos NDJSON con la operación $export de FHIR Bulk Data.
bulk-export-active-title = Exportaciones
bulk-export-new = Nueva exportación
bulk-export-unavailable = El backend de almacenamiento no aloja el settings store; no se pueden rastrear los trabajos de exportación.
bulk-export-scope = ¿Qué desea exportar?
bulk-export-scope-system = Todo
bulk-export-scope-system-hint = El servidor completo — cada tipo de recurso que seleccione abajo.
bulk-export-scope-patient = Pacientes
bulk-export-scope-patient-hint = Todos los pacientes, o solo los que elijas, con los registros que les pertenecen. Nada ajeno a pacientes.
bulk-export-scope-group = Grupo
bulk-export-scope-group-hint = Solo los miembros de una cohorte ya definida.
bulk-export-field-group-id = ID del grupo
bulk-export-field-group-id-hint = Requerido para el alcance Grupo: el id del Group FHIR a exportar.
bulk-export-field-patients = Pacientes
bulk-export-field-patients-placeholder = Buscar pacientes
bulk-export-field-patients-hint = Busca por nombre, apellido o identificador exacto. Déjalo vacío para exportar todos los pacientes.
bulk-export-field-patients-fallback-placeholder = IDs FHIR de pacientes
bulk-export-field-patients-fallback-hint = Ingresa IDs lógicos FHIR exactos separados por comas o saltos de línea. Déjalo vacío para exportar todos los pacientes.
bulk-export-field-patients-id-only-hint = Busca por ID FHIR exacto. Déjalo vacío para exportar todos los pacientes.
bulk-export-patient-options-empty = No se encontraron pacientes coincidentes.
bulk-export-patient-invalid = Ingresa solo IDs lógicos de Patient válidos, separados por comas o saltos de línea.
bulk-export-field-name = Nombre
bulk-export-field-name-placeholder = Registro de diabetes 2024
bulk-export-name-required = Ingresa un nombre para esta exportación.
bulk-export-types = Tipos de recurso
bulk-export-all-resources = Todos los recursos
bulk-export-narrow = Acotar
bulk-export-field-elements = Elementos FHIR
bulk-export-field-type-filter = Filtro por tipo
bulk-export-field-since = Desde
bulk-export-since-all = Todo el tiempo
bulk-export-since-day = Último día
bulk-export-since-week = Últimos 7 días
bulk-export-since-month = Últimas 4 semanas
bulk-export-since-custom = Personalizado
bulk-export-field-since-custom = Instante personalizado
bulk-export-since-invalid = Ingresa un instante FHIR válido, como 2026-08-01T00:00:00Z.
bulk-export-field-until = Hasta
bulk-export-field-until-hint = Límite superior opcional. RFC 3339, p. ej. 2026-08-01T00:00:00Z.
bulk-export-until-before-since = «Hasta» no puede ser anterior a «Desde».
bulk-export-window-since = Desde
bulk-export-window-until = Hasta
bulk-export-start = Iniciar exportación
bulk-export-clear = Limpiar
bulk-export-files-count = { $count ->
    [one] { $count } archivo
   *[other] { $count } archivos
}
bulk-export-summary-count = { $count ->
    [one] { $count } exportación
   *[other] { $count } exportaciones
} · { $running } en curso
bulk-export-none = Aún no hay exportaciones. Use Nueva exportación para iniciar una.
bulk-export-status-in-progress = En curso
bulk-export-status-complete = Completada
bulk-export-status-failed = Fallida
bulk-export-status-cancelled = Cancelada
bulk-export-progress = Progreso
bulk-export-progress-waiting = Esperando el primer reporte de estado…
bulk-export-writing = Escribiendo { $name }
bulk-export-types-progress = { $done } de { $total } tipos
bulk-export-files = Archivos
bulk-export-finished-in = terminada en
bulk-export-error = Error
bulk-export-cancel = Cancelar
bulk-export-cancel-refused = El servidor rechazó la cancelación:
bulk-export-retry = Reintentar
bulk-export-download-all = Descargar todos los recursos
bulk-export-download-all-aria = Descargar todos los recursos de { $name }
bulk-export-delete = Eliminar
bulk-export-delete-aria = Eliminar la exportación { $name }
bulk-export-delete-warning = ¿Eliminar { $name } y sus archivos de salida del servidor? Esta acción no se puede deshacer.
bulk-export-delete-confirm = Eliminar exportación
bulk-export-delete-cancel = Conservar exportación
bulk-export-delete-error = No se pudo eliminar la exportación de forma segura. Se conservó la tarjeta para que pueda volver a intentarlo.

# Página CapabilityStatement (#653)
cap-title = Declaración de capacidades
cap-lede = Lo que este servidor hace ahora mismo, para el tenant y la versión FHIR seleccionados — compuesto en vivo desde /metadata.
cap-summary-heading = Resumen del servidor
cap-summary-description = Descripción
cap-summary-url = URL base
cap-summary-fhir-version = Versión FHIR
cap-summary-status = Estado
cap-summary-kind = Tipo
cap-summary-date = Fecha
cap-summary-formats = Formatos
cap-interactions-heading = Interacciones de sistema
cap-transaction-note = transaction se anuncia porque el backend activo soporta transacciones atómicas; batch está siempre disponible.
cap-role-matrix = Ver la matriz de roles de backend.
cap-operations-heading = Operaciones
cap-col-operation = Operación
cap-col-definition = Definición
cap-resources-heading = Capacidades por recurso
cap-filter-placeholder = Filtrar tipos…
cap-col-type = Tipo
cap-col-interactions = Interacciones
cap-col-search-params = Parámetros de búsqueda
cap-col-includes = Includes
cap-col-revincludes = Revincludes
cap-resources-empty = Ningún tipo de recurso coincide con el filtro.
cap-raw-toggle = CapabilityStatement en bruto (JSON)
cap-raw-load = Abrir JSON sin formato
cap-raw-loading = Cargando JSON resaltado…
cap-raw-plain = Alternativa en JSON sin formato. El resaltado se carga de forma incremental cuando JavaScript está disponible.
cap-json-load-error = No se pudo cargar esta sección JSON. Ciérrala y vuelve a abrirla para reintentar.
cap-json-truncated = visualización truncada
cap-json-path-too-deep = se alcanzó la profundidad máxima de expansión
cap-json-pagination-label = Elementos JSON
cap-json-page-prev = Anterior
cap-json-page-next = Siguiente
cap-json-collapse-all = Colapsar todo
cap-json-expand-all = Expandir todo
cap-json-expanding = Expandiendo el JSON visible…
cap-json-expand-complete = Todo el JSON está expandido.
cap-json-expand-partial = Se expandieron las páginas actuales hasta el límite seguro. Los nodos restantes permanecen colapsados.
cap-json-limit-reached = Se alcanzó el límite de visualización del JSON. Colapsa otra rama antes de cargar más.
cap-json-expand-error = No se pudo expandir el árbol JSON. Se conservó la vista actual; inténtalo de nuevo.
cap-unavailable = No se pudo obtener la CapabilityStatement del servidor — la autollamada puede necesitar un token saliente cuando la autenticación está activada.

## Etiquetas genéricas de desplegable JSON (#898) — usadas por las respuestas raw en HTS-UI.
## Las claves cap-* anteriores se mantienen específicamente para CapabilityStatement.
json-fold-loading = Cargando JSON…
json-fold-load-error = No se pudo cargar esta sección JSON. Colapse y vuelva a abrir para reintentar.
json-fold-open-plain = Abrir JSON plano
json-fold-plain = JSON plano de reserva. El resaltado se carga incrementalmente cuando JavaScript está disponible.

## Stubs de la sección SQL on FHIR (#649)


sql-vd-title = Definiciones de vistas
sql-vd-lede = Crea y gestiona las ViewDefinitions con las que SQL on FHIR aplana recursos.

sql-queries-title = Consultas SQL
sql-queries-lede = SQL ad hoc sobre las tablas planas que producen tus view definitions. Los resultados se actualizan mientras escribes.

sql-views-title = Vistas SQL
sql-views-lede = Vistas con nombre que otras consultas y exportaciones pueden seleccionar. Define el SQL una vez; las columnas que produce se convierten en una tabla que cualquiera puede usar.

sql-export-title = Exportación SQL
sql-export-lede = Ejecuta ViewDefinitions, consultas SQL y vistas SQL almacenadas como un trabajo $sql-export de larga duración y descarga los archivos de salida.
sql-export-active-title = Exportaciones SQL

## Espacio de definiciones de vistas (#649)

vd-new = Crear nueva
vd-new-title = Nueva definición de vista
vd-rail-label = Definiciones de vistas
vd-rail-heading = Definiciones de vistas
vd-rail-all-heading = Todas las definiciones de vistas
vd-filter = Filtrar vistas
vd-none = Aún no hay definiciones de vistas.
vd-empty-lede = Crea tu primera ViewDefinition con «Crear nueva».
vd-degraded = No se pudo cargar la lista de definiciones de vistas.
vd-saved = Guardado.
vd-run-failed = No se pudo ejecutar la vista.
vd-save = Guardar
vd-duplicate = Duplicar
sql-duplicate-read-failed = No se pudieron comprobar los artefactos existentes antes de duplicar: { $reason }
sql-duplicate-incomplete = No se pudieron comprobar todos los artefactos existentes. La copia no se guardó; inténtelo de nuevo cuando se pueda leer el catálogo completo.
vd-delete = Eliminar
vd-delete-confirm = ¿Eliminar la definición de vista «{ $name }»? Esta acción no se puede deshacer.
vd-delete-failed = No se pudo eliminar la definición de vista.
vd-json-heading = Definición (JSON)
vd-run-hint = Se ejecuta mientras escribes: los resultados siguen la definición actual, esté guardada o no
vd-results-heading = Resultados
vd-results-empty = La vista no produjo filas.
vd-results-meta = { $rows } filas · { $ms } ms
vd-results-stale = última ejecución exitosa
vd-pagination-label = Páginas de definiciones de vistas
vd-page-prev = Anterior
vd-page-next = Siguiente

## Mensajes y arreglos del lint de ViewDefinition (#821): localización propia
## del handler `/lint` para `helios_sof::lint::Diagnostic` — code + args,
## nunca `Diagnostic.message` (siempre en inglés, sin localizar). Las claves
## `vd-lint-*` llevan el nombre kebab-case de `DiagnosticCode`; el diagnóstico
## `missing-required`/`wrong-type` de la elección value[x] de una constante
## lleva `$variant` ("missing"/"multiple") en vez de los `$key`/`$expected`+
## `$found` genéricos, seleccionado abajo. Cada clave `vd-fix-*` etiqueta un
## `Fix`, mostrado junto al diagnóstico como una acción de un clic.
vd-lint-not-a-view-definition = No es un ViewDefinition: se encontró { $found }
vd-lint-unknown-key = Clave desconocida "{ $key }"
vd-lint-missing-required = { $variant ->
    [missing] Una constante debe establecer exactamente un valor
   *[other] Falta la clave obligatoria "{ $key }"
}
vd-lint-wrong-type = { $variant ->
    [multiple] Una constante solo puede establecer un valor
   *[other] Se esperaba { $expected }, se encontró { $found }
}
vd-lint-empty-required = La clave obligatoria "{ $key }" no puede estar vacía
vd-lint-duplicate-column-name = Nombre de columna duplicado "{ $name }"
vd-lint-multiple-iteration-directives = Un select solo puede establecer una de forEach, forEachOrNull, repeat (se encontraron { $keys })
vd-lint-select-without-output = Un select debe tener al menos uno de column, select o unionAll
vd-lint-fhirpath-syntax = Sintaxis FHIRPath: { $detail }
vd-lint-undeclared-constant = Constante no declarada "%{ $name }"
vd-lint-unknown-resource-type = Tipo de recurso desconocido "{ $found }"
vd-fix-rename-key = Renombrar a "{ $to }"
vd-fix-remove-key = Quitar "{ $key }"
vd-fix-set-string = Establecer en "{ $value }"

## Completado de ViewDefinition (#821): `vd-editor.js` lo lee una vez desde
## `data-msg-required` de `#vd-editor-grid` — el marcador que lleva el
## elemento de completado de una clave estructural obligatoria, añadido a su
## texto `detail` en el cliente (nunca lo envía `/complete`, que no traduce).
vd-complete-required = obligatorio

## Aviso de guardado con errores del editor de ViewDefinition (#821):
## `vd-editor.js` elige cuál de estos renderiza `data-msg-save-errors-one`/
## `-other` de `#vd-editor-grid` y sustituye el marcador literal `{count}`
## (el valor que se le dio a `$count`) por el número real — el texto de
## `window.confirm` no tiene traducción propia, así que esta es la única
## forma de traducirlo. Se muestra solo al enviar como Save (no Duplicate)
## mientras la última pasada de lint completada aún tenga algún error.
vd-save-with-errors-one = Esta definición de vista aún tiene { $count } error. ¿Guardar de todas formas?
vd-save-with-errors-other = Esta definición de vista aún tiene { $count } errores. ¿Guardar de todas formas?

## Partial de resultados de $sql-run (#752): compartido por los playgrounds
## de View Definitions, SQL Queries y SQL Views (#839).

sql-run-export = Exportar como archivos

## Espacios de consultas y vistas SQL (#649, layout editor-first #839)

sql-queries-new-title = Nueva consulta SQL
sql-views-new-title = Nueva vista SQL
sql-queries-rail-all-heading = Todas las consultas SQL
sql-views-rail-all-heading = Todas las vistas SQL
sql-queries-chip = Consulta SQL
sql-views-chip = Vista SQL
sql-queries-filter = Filtrar consultas
sql-views-filter = Filtrar vistas
sql-queries-rail-empty = Aún no hay consultas.
sql-views-rail-empty = Aún no hay vistas.
sql-queries-empty-title = Aún no hay consultas SQL
sql-views-empty-title = Aún no hay vistas SQL
sql-queries-empty-lede = Escribe tu primera consulta con «Crear nueva». Se ejecuta contra las tablas planas de cada view definition activa.
sql-views-empty-lede = Define tu primera vista con «Crear nueva». Sus columnas se convierten en una tabla que otras consultas y exportaciones pueden seleccionar.
sql-queries-editor-heading = SQL
sql-views-editor-heading = Definición de vista (SQL)
sql-queries-results-heading = Resultados
sql-views-results-heading = Vista previa
sql-queries-run-failed = No se pudo ejecutar la consulta.
sql-views-run-failed = No se pudo ejecutar la vista.
lib-degraded = No se pudo cargar la lista de bibliotecas.
lib-run-hint = Se ejecuta mientras escribes: los resultados siguen el SQL actual, esté guardado o no
lib-sql-attachment-unreadable = Adjunto SQL ilegible: la tarjeta SQL conserva su último texto legible; escribir aquí lo repara
lib-delete-confirm = ¿Eliminar «{ $name }»? Esta acción no se puede deshacer.
lib-delete-failed = No se pudo eliminar la biblioteca.
lib-details-heading = Detalles
lib-details-json-heading = Library (JSON)
lib-details-json-note = El adjunto SQL (content[].data) forma parte de este documento. La tarjeta SQL de abajo edita el mismo adjunto.
lib-details-new-lede = Cámbiale el nombre y haz que relatedArtifact[0] apunte a una ViewDefinition que exista.
lib-save-wrong-kind = El tipo de Library de SQL on FHIR debe ser «{ $code }» para guardarla aquí.
lib-save-view-parameters = Un SQL View no puede declarar parámetros (perfil SQLView, Library.parameter 0..0). Quita parameter[] en Detalles.

## Tarjeta Parameters (#841, solo SQL Query)

lib-params-heading = Parameters
lib-params-meta = declarado en la Library · los valores de abajo son solo para esta ejecución
lib-params-empty = No hay parámetros declarados. Añade uno aquí o en Detalles.
lib-params-hint = :{ $name } se usa en el SQL pero no está declarado en la Library.
lib-params-declare = Declarar :{ $name }
lib-params-add-toggle = Añadir parámetro
lib-params-add-name-label = Nombre
lib-params-add-type-label = Tipo
lib-params-add-submit = Añadir
lib-params-add-invalid-name = Los nombres de parámetro deben coincidir con ^[A-Za-z][A-Za-z0-9_]*$
lib-params-add-duplicate = Ya existe un parámetro llamado { $name }
lib-params-add-unknown-type = Tipo de parámetro desconocido
lib-run-waiting = Esperando un valor para { $names } — los resultados de abajo son de la última ejecución correcta.
lib-run-unknown-table = Tabla desconocida { $name } — línea { $line }. Decláralo en Reads from o corrige el nombre. Tu SQL no ha cambiado; los resultados de abajo son de la última ejecución correcta.
lib-run-unknown-table-more = Tabla desconocida { $name } — línea { $line }.

## Panel de tablas (#842, ambos tipos) — Reads from / Used by / Columns

lib-tables-heading = Lee de
lib-tables-no-alias = (sin etiqueta)
lib-tables-empty = Aún no hay tablas declaradas.
lib-tables-kind-view-definition = ViewDefinition
lib-tables-target-not-found = No encontrado
lib-tables-target-not-found-detail = Ninguna ViewDefinition ni SQL View responde a { $resource }. Corrige el canónico en Details o quita la fila.
lib-tables-target-not-a-table = No es una tabla
lib-tables-target-not-a-table-detail = Solo se puede leer una ViewDefinition o una SQL View.
lib-tables-add-table-label = Tabla
lib-tables-add-table-placeholder = Añadir una tabla…
lib-tables-add-table-fallback-placeholder = ViewDefinition/{"{"}id{"}"} o Library/{"{"}id{"}"}
lib-tables-alias-label = Alias
lib-tables-add-submit = Añadir
lib-tables-add-hint = Busca una view definition o SQL view; el alias toma su nombre por defecto.
lib-tables-add-error-required = Elige una view definition o una SQL view
lib-tables-add-error-alias-required = El alias es obligatorio
lib-tables-add-error-alias-invalid = El alias debe coincidir con ^[A-Za-z][A-Za-z0-9_]*$
lib-tables-add-error-alias-duplicate = El alias { $alias } ya está declarado
lib-tables-options-empty = Sin coincidencias.
lib-tables-unknown = Tabla desconocida
lib-tables-unknown-detail = Se usa en el SQL pero no está declarada. Elige una view definition o SQL view, o corrige el nombre.
lib-tables-declare = Declarar { $name }
lib-tables-declare-short = Declarar
lib-tables-remove-row = Quitar { $alias }

lib-used-by-heading = Usado por
lib-used-by-empty = Nada lo usa todavía.
lib-used-by-export-kind = Exportación SQL

lib-columns-heading = Columnas
lib-columns-meta-query = lo que produce la consulta
lib-columns-meta-view = lo que produce la vista
lib-columns-empty-query = Ejecuta la consulta para ver sus columnas.
lib-columns-empty-view = Ejecuta la vista para ver sus columnas.
lib-columns-col-name = Columna
lib-columns-col-type = Tipo
lib-columns-col-from = De

## Páginas de exportación SQL (#649, #833)

export-start-failed = No se pudo iniciar la exportación.
export-format = Formato de salida
export-start = Iniciar exportación

## Formulario de creación del generador de exportaciones SQL (#834)

sql-export-name-placeholder = Archivos planos mensuales de pacientes
sql-export-subjects-legend = ¿Qué vas a exportar?
sql-export-subjects-heading = Elementos
sql-export-filter-all = Todos
sql-export-filter-view-definitions = ViewDefinitions
sql-export-filter-queries = Consultas
sql-export-filter-views = Vistas
sql-export-filter-subjects = Filtrar elementos
sql-export-select-all = Seleccionar todo
sql-export-col-subject = Elemento
sql-export-col-kind = Tipo
sql-export-col-status = Estado
sql-export-selected-count = { $selected } de { $total } seleccionados
sql-export-format-ndjson-hint = Un objeto JSON por fila. Se transmite bien hacia otras canalizaciones.
sql-export-format-csv-hint = Fila de encabezado más una línea por fila. Se abre en cualquier hoja de cálculo.
sql-export-format-json-hint = Un único arreglo JSON por salida. El más fácil de inspeccionar a mano.
sql-export-format-parquet-hint = Columnar y tipado. El mejor para motores de análisis.
sql-export-empty-heading = Aún no hay nada que exportar
sql-export-empty-body = Cada ViewDefinition, consulta SQL y vista SQL almacenada aparece aquí como un elemento exportable. Crea uno primero.
sql-export-filter-empty = Ningún elemento coincide

## Generador de exportaciones SQL — "Acotar" y "Avanzado" (#836)

sql-export-narrow-title = Acotar
sql-export-narrow-meta = opcional — déjalo vacío para exportar todo
sql-export-field-patients = Pacientes
sql-export-field-patients-placeholder = Buscar pacientes
sql-export-field-patients-hint = Busca por nombre, apellido o identificador exacto. Déjalo vacío para exportar todos los pacientes.
sql-export-field-patients-fallback-placeholder = IDs FHIR de pacientes
sql-export-field-patients-fallback-hint = Ingresa IDs lógicos FHIR exactos separados por comas o saltos de línea. Déjalo vacío para exportar todos los pacientes.
sql-export-field-patients-id-only-hint = Busca por ID FHIR exacto. Déjalo vacío para exportar todos los pacientes.
sql-export-field-groups = Grupos
sql-export-field-groups-placeholder = Buscar grupos
sql-export-field-groups-hint-r4 = Busca por ID FHIR exacto o identificador.
sql-export-field-groups-hint-r5 = Busca por nombre, ID FHIR exacto o identificador.
sql-export-field-groups-fallback-placeholder = IDs FHIR de grupos
sql-export-field-groups-fallback-hint = Ingresa IDs FHIR o identificadores exactos separados por comas o saltos de línea.
sql-export-group-options-empty = No se encontraron grupos coincidentes.
sql-export-field-since = Desde
sql-export-since-all = Todo el tiempo
sql-export-since-day = Último día
sql-export-since-week = Últimos 7 días
sql-export-since-month = Últimas 4 semanas
sql-export-since-custom = Personalizado
sql-export-field-since-custom = Instante personalizado
sql-export-since-invalid = Ingresa un instante FHIR válido, como 2026-08-01T00:00:00Z.
sql-export-patient-invalid = Ingresa solo IDs lógicos de Patient válidos, separados por comas o saltos de línea.
sql-export-group-invalid = Ingresa solo IDs lógicos de Group válidos, separados por comas o saltos de línea.
sql-export-advanced = Avanzado
sql-export-advanced-meta = id de seguimiento · encabezado CSV
sql-export-field-tracking-id = Id de seguimiento
sql-export-field-tracking-id-hint = Se repite en el manifiesto de finalización como clientTrackingId.
sql-export-tracking-id-too-long = El id de seguimiento debe tener 200 caracteres o menos.
sql-export-field-header = Incluir una fila de encabezado
sql-export-field-header-hint = (solo CSV — se ignora para otros formatos)

## Lista de exportaciones SQL activas y tarjetas de trabajo (#833)

sql-export-new = Nueva exportación SQL
sql-export-unavailable = El backend de almacenamiento no aloja el settings store; no se pueden rastrear los trabajos de exportación SQL.
sql-export-none = Aún no hay exportaciones SQL. Use Nueva exportación SQL para iniciar una.
sql-export-store-error = La exportación se inició, pero no se pudo agregar a esta lista. Id del trabajo:
sql-export-summary-count = { $count ->
    [one] { $count } exportación
   *[other] { $count } exportaciones
} · { $running } en curso
sql-export-select-subject = Seleccione al menos un elemento.
sql-export-unknown-subject = Uno o más elementos seleccionados ya no están disponibles. Actualice la página e inténtelo de nuevo.
sql-export-cancelled-reason = el servidor ya no conoce este trabajo
sql-export-status-in-progress = En curso
sql-export-status-complete = Completada
sql-export-status-failed = Fallida
sql-export-status-cancelled = Cancelada
sql-export-view-files = Ver archivos
sql-export-cancel = Cancelar
sql-export-retry = Reintentar
sql-export-run-again = Volver a ejecutar
sql-export-remove = Quitar de la lista
sql-export-more-actions = Más acciones
sql-export-copy-job-id = Copiar id del trabajo
sql-export-copied = Copiado
sql-export-progress-waiting = Esperando el primer reporte de estado…
sql-export-writing = Escribiendo { $name }
sql-export-subjects-progress = { $total ->
    [one] { $done } de { $total } elemento
   *[other] { $done } de { $total } elementos
}
sql-export-started = iniciada
sql-export-finished-in = terminada en
sql-export-cancelled-at = cancelada a las
sql-export-status-unavailable = estado no disponible
sql-export-format-ndjson = NDJSON
sql-export-format-csv = CSV
sql-export-format-json = JSON
sql-export-format-parquet = Parquet
sql-export-subjects-count = { $count ->
    [one] { $count } elemento
   *[other] { $count } elementos
}
sql-export-kind-view-definition = { $count ->
    [one] { $count } ViewDefinition
   *[other] { $count } ViewDefinitions
}
sql-export-kind-sql-query = { $count ->
    [one] { $count } SQL Query
   *[other] { $count } SQL Queries
}
sql-export-kind-sql-view = { $count ->
    [one] { $count } SQL View
   *[other] { $count } SQL Views
}
sql-export-files-count = { $count ->
    [one] { $count } archivo
   *[other] { $count } archivos
}

## Página de detalle del trabajo (#835)

sql-export-detail-finished = Finalizada
sql-export-detail-failed = Fallida
sql-export-detail-cancelled = Cancelada
sql-export-detail-started = Iniciada
sql-export-detail-after = luego de
sql-export-failure-subject = La exportación se detuvo en el elemento
sql-export-failure-generic = La exportación falló:
sql-export-detail-job-heading = Trabajo
sql-export-detail-field-job-id = Id del trabajo
sql-export-detail-field-format = Formato
sql-export-detail-field-started = Iniciado
sql-export-detail-field-duration = Duración
sql-export-detail-field-subjects = Elementos
sql-export-detail-header-included = con fila de encabezado
sql-export-detail-header-omitted = sin fila de encabezado
sql-export-detail-outputs-heading = Archivos de salida
sql-export-detail-col-output = Salida
sql-export-detail-col-subject = Elemento
sql-export-detail-col-files = Archivos
sql-export-detail-outputs-empty = El trabajo no produjo archivos de salida.
sql-export-file-fallback = Archivo { $n }

## #837: valores de parámetros por SQL Query en el formulario de SQL Export
sql-export-param-count = { $count ->
    [one] { $count } parámetro
   *[other] { $count } parámetros
}
sql-export-param-required = Este valor es obligatorio.
sql-export-param-type-mismatch = Se esperaba un valor de tipo { $type }.
sql-export-param-required-chip = obligatorio
sql-export-toggle-values = Mostrar u ocultar valores
sql-export-values-missing-one = falta { $count } valor
sql-export-values-missing-other = faltan { $count } valores

## partials/sql_parameter_fields.html (#837) — compartido con el formulario
## de parámetros de la página de SQL Query
sql-param-default = predeterminado

## UI administrativa de HTS (crates/hts-ui) — stubs de Phase 1
##
## Las claves de la UI de HTS siguen la convención
## hts-<pagina>-<rol>-<control>. Estos stubs cubren el layout base, la nav
## lateral y el placeholder del dashboard de la Phase 1 blocker slice. Deben
## mantenerse en paridad con en/de/main.ftl.

-hts-app-name = Servidor de Terminología Helios
hts-app-title = { -hts-app-name }

hts-nav-section-work = Terminología
hts-nav-section-tools = Herramientas
hts-nav-section-server = Servidor
hts-nav-home = Inicio
hts-nav-code-systems = Sistemas de códigos
hts-nav-value-sets = Conjuntos de valores
hts-nav-concept-maps = Mapas de conceptos
hts-nav-operations = Operaciones
hts-nav-import = Importar

hts-fhir-version-heading = Versión FHIR
hts-fhir-version = FHIR { $version }

hts-home-title = Inicio
hts-home-subtitle = Estado del servidor de terminología, inventario del catálogo y acciones rápidas.

## Filas del panel (encabezados ocultos visualmente para lectores de pantalla).

hts-home-row-status = Estado del servidor

## Tarjetas del panel.

hts-home-tile-status = Estado
hts-home-tile-uptime = Tiempo activo
hts-home-tile-loaded-systems = Sistemas de códigos cargados
hts-home-tile-loaded-systems-hint = De TerminologyCapabilities.codeSystem[]
hts-home-tile-requests = Solicitudes
hts-home-tile-metrics-hint = Desde el arranque del servidor

## Gráfico de tasa de solicitudes de Inicio (design doc §7.1). Traza una tasa
## calculada a partir de los contadores acumulados de `/metrics`, muestreada
## solo mientras esta página está abierta, por lo que cada estado «nada que
## dibujar» necesita su propio texto.

hts-home-chart-title = Solicitudes por minuto
hts-home-chart-window = Ventana temporal del gráfico
hts-home-chart-series = Clase de estado
hts-home-chart-window-15m = 15 min
hts-home-chart-window-1h = 1 h
hts-home-chart-window-6h = 6 h
hts-home-chart-series-all = Todas
hts-home-chart-series-2xx = 2xx
hts-home-chart-series-4xx = 4xx
hts-home-chart-series-5xx = 5xx
hts-home-chart-empty-unreachable = /metrics no está accesible: no llegan muestras nuevas.
hts-home-chart-empty-none = Aún no se ha recogido ninguna muestra.
hts-home-chart-empty-first = Recogiendo el primer intervalo: una tasa necesita dos muestras.
hts-home-chart-empty-window = No hay muestras en esta ventana. El muestreo solo se ejecuta mientras esta página está abierta.
hts-home-chart-axis-now = ahora
hts-home-chart-axis-minutes = -{ $n } min
hts-home-chart-axis-hours = -{ $n } h

## Valores del campo `status` de /health, con clave para traducción.

hts-home-status-ok = OK

## Banner degradado (contrato del design doc §7).

hts-degraded-title = El backend de terminología no está totalmente disponible
hts-degraded-body = Algunas tarjetas se ocultan hasta que HTS vuelva a estar accesible. Los controles interactivos se deshabilitan en las páginas afectadas.
hts-degraded-reason-client-build = No se pudo construir el cliente HTTP hacia HTS.
hts-degraded-reason-upstream-down = No se puede alcanzar el servidor de terminología.
hts-degraded-reason-upstream-timeout = El servidor de terminología no respondió a tiempo.
hts-degraded-reason-upstream-error = El servidor de terminología devolvió un error.
hts-degraded-reason-upstream-shape = El servidor de terminología devolvió una respuesta con forma inesperada.
hts-degraded-reason-bootstrapping = El servidor de terminología todavía está cargando sus datos iniciales.
hts-degraded-reason-unknown = El servidor de terminología no está disponible temporalmente.

## Chip de dialecto (topbar, displayLanguage / Accept-Language de sesión — §7.1).


## Partial de OperationOutcome (compartido — §7 / §11).

hts-outcome-severity = Severidad: { $severity }
hts-outcome-request-id = Id de solicitud: { $id }
hts-outcome-code-not-found = El recurso solicitado no fue encontrado.
hts-outcome-code-invalid = La solicitud fue rechazada por inválida.
hts-outcome-code-too-costly = La operación solicitada fue rechazada por ser demasiado costosa.
hts-outcome-code-unknown = El servidor devolvió una incidencia que la UI no reconoce.
hts-degraded-since = Desde { $timestamp }

## HTS Slice B — Navegador de CodeSystem + detalle con banco de trabajo integrado
## (design doc §7.2 + §7.3). Cada clave tiene su equivalente en en/de/main.ftl.

## Píldoras de estado de CodeSystem (usadas en el navegador y en la cabecera del detalle).

hts-cs-status-draft = borrador
hts-cs-status-active = activo
hts-cs-status-retired = retirado
hts-cs-status-unknown = desconocido

## Página del navegador de CodeSystem.

hts-cs-browser-title = Sistemas de códigos
hts-cs-browser-subtitle = Explora el catálogo de CodeSystems del servidor de terminología y abre cualquier fila para inspeccionar sus metadatos y su banco de trabajo.
hts-cs-browser-filter-legend = Filtrar CodeSystems
hts-cs-browser-filter-url = URL canónica
hts-cs-browser-filter-version = Versión
hts-cs-browser-filter-name = Nombre
hts-cs-browser-filter-title = Título
hts-cs-browser-filter-status = Estado
hts-cs-browser-filter-reset = Restablecer
hts-cs-browser-empty = Ningún CodeSystem coincide con estos filtros.
hts-cs-browser-load-more = Cargar más
hts-cs-browser-showing-count = Mostrando { $count ->
    [one] { $count } CodeSystem
   *[other] { $count } CodeSystems
}
hts-cs-browser-table-caption = CodeSystems que coinciden con los filtros activos.
hts-cs-browser-column-url = URL
hts-cs-browser-column-version = Versión
hts-cs-browser-column-title = Título
hts-cs-browser-column-status = Estado
hts-cs-browser-column-name = Nombre

## Fase 5 — Cadenas compartidas del formulario de búsqueda HTS.

hts-search-rail-label = Filtros de búsqueda
hts-search-rail-heading = Filtros
hts-facet-status-any = Cualquier estado

## Página de detalle del CodeSystem.

hts-cs-detail-title = { $name } · CodeSystem
hts-cs-detail-title-fallback = CodeSystem
hts-cs-detail-eyebrow = CodeSystem
hts-cs-detail-section-identity = Identidad
hts-cs-detail-section-content = Contenido
hts-cs-detail-content-mode = Modo de contenido
hts-cs-detail-count = Cantidad de conceptos
hts-cs-detail-publisher = Publicador
hts-cs-detail-jurisdiction = Jurisdicción
hts-cs-detail-supersedes = Reemplaza a
hts-cs-detail-superseded-by = Reemplazado por
hts-cs-detail-tabs-label = Secciones del banco de trabajo del CodeSystem
hts-cs-detail-tab-lookup = Consulta
hts-cs-detail-tab-validate = Validar
hts-cs-detail-tab-subsumes = Subsunción
hts-cs-detail-result-empty = Ejecuta la operación para ver su resultado aquí.

## Formulario y resultados de $lookup.

hts-cs-lookup-heading = Consultar un concepto
hts-cs-lookup-code = Código
hts-cs-lookup-version = Versión
hts-cs-lookup-display-language = Idioma de visualización
hts-cs-lookup-display-language-placeholder = p. ej. es-ES
hts-cs-lookup-properties-legend = Propiedades
hts-cs-lookup-designations = Designaciones
hts-cs-lookup-properties = Propiedades
hts-cs-lookup-no-match = HTS no devolvió ningún concepto coincidente.

## Formulario y resultados de $validate-code.

hts-cs-validate-heading = Validar un código
hts-cs-validate-mode-legend = Modo de entrada
hts-cs-validate-mode-code = Código simple
hts-cs-validate-mode-coding = Coding
hts-cs-validate-code-legend = Código simple
hts-cs-validate-code = Código
hts-cs-validate-display = Visualización
hts-cs-validate-coding-legend = Coding
hts-cs-validate-coding-system = sistema
hts-cs-validate-coding-code = código
hts-cs-validate-coding-display = visualización
hts-cs-validate-badge-true = válido
hts-cs-validate-badge-false = inválido
hts-cs-validate-message = Mensaje

## Formulario y resultados de $subsumes.

hts-cs-subsumes-heading = Probar subsunción
hts-cs-subsumes-scoped-system = Sistema (fijo)
hts-cs-subsumes-code-a = Código A
hts-cs-subsumes-code-b = Código B
hts-cs-subsumes-outcome-equivalent = Los códigos son equivalentes.
hts-cs-subsumes-outcome-subsumes = El código A subsume al código B.
hts-cs-subsumes-outcome-subsumed-by = El código A está subsumido por el código B.
hts-cs-subsumes-outcome-not-subsumed = Ninguno subsume al otro.

## Cromo compartido del banco de trabajo (reutilizado por Slice C/D/E).

hts-workbench-run = Ejecutar
hts-workbench-raw-response = Solicitud y respuesta sin procesar
hts-workbench-copy-url = URL de la solicitud
hts-workbench-request-body = Cuerpo de la solicitud
hts-workbench-response-body = Cuerpo de la respuesta

## Razón degradada adicional para 404 al leer CS (states matrix §7.3).

hts-degraded-reason-upstream-not-found = El servidor de terminología no encontró ese recurso.

## HTS Slice C — Navegador de ValueSet + detalle con banco de trabajo $expand
## (design doc §7.4 + §7.4.1). Cada clave aquí tiene par en en/de/main.ftl.

## Píldoras de estado de ValueSet.

hts-vs-status-draft = borrador
hts-vs-status-active = activo
hts-vs-status-retired = retirado
hts-vs-status-unknown = desconocido

## Página del navegador VS.

hts-vs-browser-title = Conjuntos de valores
hts-vs-browser-subtitle = Explora el catálogo de ValueSets del servidor de terminología y abre cualquier fila para inspeccionar sus metadatos o ejecutar una expansión.
hts-vs-browser-filter-legend = Filtrar ValueSets
hts-vs-browser-filter-url = URL canónica
hts-vs-browser-filter-version = Versión
hts-vs-browser-filter-name = Nombre
hts-vs-browser-filter-title = Título
hts-vs-browser-filter-status = Estado
hts-vs-browser-filter-reset = Restablecer
hts-vs-browser-empty = Ningún ValueSet coincide con estos filtros.
hts-vs-browser-load-more = Cargar más
hts-browser-of-total = de { $total }
hts-browser-pager = Páginas
hts-browser-page-prev = Página anterior
hts-browser-page-next = Página siguiente
hts-browser-goto-offset = Ir al desplazamiento
hts-browser-goto = Ir
hts-vs-expand-count-hint = Hasta 500 conceptos por página; use Desplazamiento o «Cargar más» para el resto.
hts-vs-browser-showing-count = Mostrando { $count ->
    [one] { $count } ValueSet
   *[other] { $count } ValueSets
}
hts-vs-browser-table-caption = ValueSets que coinciden con los filtros activos.
hts-vs-browser-column-url = URL
hts-vs-browser-column-version = Versión
hts-vs-browser-column-title = Título
hts-vs-browser-column-status = Estado
hts-vs-browser-column-name = Nombre

## Página de detalle VS.

hts-vs-detail-title = { $name } · ValueSet
hts-vs-detail-title-fallback = ValueSet
hts-vs-detail-eyebrow = ValueSet
hts-vs-detail-section-identity = Identidad
hts-vs-detail-section-governance = Gobernanza
hts-vs-detail-publisher = Publicador
hts-vs-detail-jurisdiction = Jurisdicción
hts-vs-detail-immutable = Inmutable
hts-vs-detail-immutable-yes = sí
hts-vs-detail-immutable-no = no
hts-vs-detail-purpose = Propósito
hts-vs-detail-copyright = Derechos de autor
hts-vs-detail-tabs-label = Secciones del banco de trabajo del ValueSet
hts-vs-detail-tab-expand = Expandir
hts-vs-detail-result-empty = Ejecuta la operación para ver su resultado aquí.

## Formulario y resultados de $expand.

hts-vs-expand-heading = Expandir este ValueSet
hts-vs-expand-scoped-valueset = ValueSet (fijo)
hts-vs-expand-filter = Filtro
hts-vs-expand-filter-placeholder = código o texto de visualización
hts-vs-expand-count = count
hts-vs-expand-offset = offset
hts-vs-expand-display-language = Idioma de visualización
hts-vs-expand-display-language-placeholder = p. ej. es-ES
hts-vs-expand-flags-legend = Opciones
hts-vs-expand-active-only = Solo conceptos activos
hts-vs-expand-include-designations = Incluir designaciones
hts-vs-expand-mode-legend = Modo del resultado
hts-vs-expand-mode-flat = Plano
hts-vs-expand-mode-tree = Árbol
hts-vs-expand-use-supplement-legend = Suplementos aplicados
hts-vs-expand-use-supplement-placeholder = URL canónica
hts-vs-expand-advanced-summary = Avanzado
hts-vs-expand-date = Fecha
hts-vs-expand-date-placeholder = ISO 8601 (p. ej. 2025-06-01)
hts-vs-expand-property-legend = Propiedades
hts-vs-expand-property-placeholder = código de propiedad
hts-vs-expand-tx-resource-legend = tx-resource
hts-vs-expand-tx-resource-placeholder = URL canónica o referencia
hts-vs-expand-system-version-legend = system-version
hts-vs-expand-system-version-placeholder = sistema|versión
hts-vs-expand-check-system-version-legend = check-system-version
hts-vs-expand-force-system-version-legend = force-system-version
hts-vs-expand-default-valueset-version = default-valueset-version
hts-vs-expand-threshold = Umbral too-costly
hts-vs-expand-ceiling-tooltip = Límite superior de la UI: { $ceiling } (valores mayores se descartan)
hts-vs-expand-ceiling-note = límite: { $ceiling }
hts-vs-expand-ceiling-warning-title = Umbral por encima del límite de la UI
hts-vs-expand-ceiling-warning-body = Solicitaste el umbral { $requested }, que supera el límite de la UI — la cabecera no se adjuntó.
hts-vs-expand-ceiling-value = límite: { $ceiling }
hts-vs-expand-too-costly-title = Expansión rechazada por costosa
hts-vs-expand-too-costly-body = HTS rechazó la expansión por superar el umbral actual. Súbelo aquí abajo y reintenta, o restringe el filtro.
hts-vs-expand-raise-threshold = Elevar umbral a
hts-vs-expand-raise-submit = Reintentar
hts-vs-expand-tree-label = mostrando el árbol completo { $count ->
    [one] { $count } hoja
   *[other] { $count } hojas
}
hts-vs-expand-total-label = total { $total }
hts-vs-expand-total-unknown = total (desconocido)
hts-vs-expand-offset-label = offset { $offset }
hts-vs-expand-filter-no-match = Ningún miembro coincide con el filtro "{ $filter }".
hts-vs-expand-no-members = Esta expansión no contiene miembros.
hts-vs-expand-column-code = Código
hts-vs-expand-column-display = Visualización
hts-vs-expand-column-system = Sistema
hts-vs-expand-load-more = Cargar más
hts-vs-expand-echoed-parameters = Parámetros ecoados

## HTS Slice D — Explorador y detalle de ConceptMap con banco de trabajo
## de $translate embebido (doc. de diseño §7.5). Cada clave tiene su par
## en en/de/main.ftl.

## Estados del ConceptMap.

hts-cm-status-draft = borrador
hts-cm-status-active = activo
hts-cm-status-retired = retirado
hts-cm-status-unknown = desconocido

## Explorador de CM.

hts-cm-browser-title = Mapas de conceptos
hts-cm-browser-subtitle = Explora el catálogo de ConceptMaps del servidor de terminología y abre cualquier fila para inspeccionar sus metadatos o ejecutar una traducción.
hts-cm-browser-filter-legend = Filtrar ConceptMaps
hts-cm-browser-filter-url = URL canónica
hts-cm-browser-filter-name = Nombre
hts-cm-browser-filter-title = Título
hts-cm-browser-filter-status = Estado
hts-cm-browser-filter-hint = Las URL canónicas de origen y destino no se ofrecen como filtros: al buscar ConceptMaps, HTS solo acepta url, version, name, title y status, e ignora el resto. Filtre por URL o nombre y luego consulte la columna Mapeo.
hts-cm-browser-filter-reset = Restablecer
hts-cm-browser-empty = Ningún ConceptMap coincide con estos filtros.
hts-cm-browser-load-more = Cargar más
hts-cm-browser-showing-count = Mostrando { $count ->
    [one] { $count } ConceptMap
   *[other] { $count } ConceptMaps
}
hts-cm-browser-table-caption = ConceptMaps que coinciden con los filtros activos.
hts-cm-browser-column-url = URL
hts-cm-browser-column-title = Título
hts-cm-browser-column-status = Estado
hts-cm-browser-column-name = Nombre
hts-cm-browser-column-source = Sistema de origen
hts-cm-browser-column-target = Sistema de destino
hts-cm-browser-column-mapping = Mapeo
hts-cm-browser-mapping-source-prefix = O:
hts-cm-browser-mapping-target-prefix = D:

## Detalle de CM.

hts-cm-detail-title = { $name } · ConceptMap
hts-cm-detail-title-fallback = ConceptMap
hts-cm-detail-eyebrow = ConceptMap
hts-cm-detail-section-identity = Identidad
hts-cm-detail-section-mapping = Mapeo
hts-cm-detail-publisher = Publicador
hts-cm-detail-jurisdiction = Jurisdicción
hts-cm-detail-purpose = Propósito
hts-cm-detail-source-uri = Origen
hts-cm-detail-target-uri = Destino
hts-cm-detail-group-count = Grupos
hts-cm-detail-tabs-label = Secciones del banco de trabajo del ConceptMap
hts-cm-detail-tab-translate = Traducir
hts-cm-detail-result-empty = Ejecuta la operación para ver su resultado aquí.

## Formulario y resultados de $translate.

hts-cm-translate-heading = Traducir un código
hts-cm-translate-scoped-map = ConceptMap (fijo)
hts-cm-translate-direction-legend = Dirección
hts-cm-translate-direction-forward = Directa
hts-cm-translate-direction-reverse = Inversa
hts-cm-translate-source-legend = Codificación origen
hts-cm-translate-source-system = Sistema
hts-cm-translate-source-system-placeholder = URL canónica
hts-cm-translate-source-code = Código
hts-cm-translate-source-display = Visualización
hts-cm-translate-source-display-placeholder = opcional
hts-cm-translate-reverse-legend = Origen inverso
hts-cm-translate-target-code = Código destino
hts-cm-translate-target-code-hint = Obligatorio en modo inverso.
hts-cm-translate-target-legend = Restricciones de destino
hts-cm-translate-target-system = Sistema destino
hts-cm-translate-target-system-placeholder = URL canónica
hts-cm-translate-source-url = ValueSet origen
hts-cm-translate-source-url-placeholder = URL canónica (opcional)
hts-cm-translate-target-url = ValueSet destino
hts-cm-translate-target-url-placeholder = URL canónica (opcional)
hts-cm-translate-date = Fecha
hts-cm-translate-date-placeholder = ISO 8601 (p. ej. 2025-06-01)
hts-cm-translate-submit = Traducir
hts-cm-translate-matches-count = { $count ->
    [one] { $count } coincidencia
   *[other] { $count } coincidencias
}
hts-cm-translate-no-matches = No hay coincidencias para este origen.
hts-cm-translate-column-code = Código
hts-cm-translate-column-system = Sistema
hts-cm-translate-column-display = Visualización
hts-cm-translate-column-mapping = { $kind ->
    [equivalence] Equivalencia
    [relationship] Relación
   *[other] Mapeo
}
hts-cm-translate-column-origin = Origen

## HTS Slice E -- operaciones (design doc s7.6).






hts-vs-expand-advanced = Parametros avanzados
hts-vs-expand-total = total { $n }





## Slice F — Importacion (§7.7). Traducciones iniciales; revisar en la
## pasada de i18n (# TODO(F): review es).

hts-import-title = Importar terminologia
hts-import-heading = Importar terminologia
hts-import-help = Envia un Bundle FHIR en JSON. HTS acepta CodeSystem, ValueSet y ConceptMap en un solo POST.
hts-import-source-legend = Origen
hts-import-source-paste = Pegar JSON
hts-import-source-file = Subir archivo
hts-import-bundle-textarea-label = Bundle FHIR (JSON)
hts-import-bundle-file-label = Archivo del Bundle (JSON)
hts-import-submit = Importar
hts-import-status-empty = Aun no se envio ninguna importacion.
hts-import-status-success = Importacion completa
hts-import-status-partial = Importacion parcialmente exitosa
hts-import-status-rejected = Importacion rechazada
hts-import-status-too-large = Bundle demasiado grande
hts-import-counts-heading = Conteos por recurso
hts-import-counts-created = Creados / actualizados
hts-import-resource-code-system = CodeSystem
hts-import-resource-value-set = ValueSet
hts-import-resource-concept-map = ConceptMap
hts-import-resource-concept = Conceptos insertados
hts-import-issues-heading = { $n ->
    [one] { $n } incidencia
   *[other] { $n } incidencias
}
hts-import-too-large-hint = La peticion supero el limite del servidor. Divide el Bundle en lotes mas pequenos y reintenta.
hts-import-empty-bundle-error = Pega un Bundle JSON antes de enviar.
hts-import-invalid-json-error = El cuerpo enviado no es JSON valido.

# Importacion por pasos (V3, #551): elegir origen, revisar, resultado.
# El paso 2 no muestra recuentos: HTS solo los devuelve en la respuesta de
# POST /import.
hts-import-step-source = Elegir origen
hts-import-step-review = Revisar
hts-import-step-result = Resultado
hts-import-file-hint = Solo JSON. El archivo se lee en el navegador y se copia en el campo Bundle de abajo; no se envia nada hasta que confirmes.
hts-import-drop-hint = Suelta aqui un archivo JSON de bundle
hts-import-drop-browse = o haz clic para explorar
hts-import-reading = Leyendo archivo…
hts-import-read-failed = No se pudo leer el archivo.
hts-import-file-too-large = El archivo excede el limite de ~7,5 MiB para envio URL-codificado.
hts-import-file-loaded = Archivo cargado: {"{name}"}
hts-import-bundle-hint = El Bundle se envia a POST /import en el servidor de terminologia. Los recursos existentes se emparejan por url + version.
hts-import-review-target = Servidor destino
hts-import-review-request = Peticion
hts-import-review-accepted = Recursos aceptados
hts-import-review-accepted-value = CodeSystem, ValueSet, ConceptMap
hts-import-review-existing = Recursos existentes
hts-import-review-existing-value = Se actualizan en el sitio cuando url y version coinciden.
hts-import-review-note = No se escribe nada hasta que confirmes. El servidor informa mas abajo cuantos recursos se crearon realmente.
hts-import-counts-resource = Recurso
hts-import-raw-toggle = Respuesta sin procesar
hts-import-rejected-note = No se escribio nada en el almacen de terminologia.
hts-import-tag-success = Correcto
hts-import-tag-partial = Parcial
hts-import-tag-error = Error

## Slice G — Diagnostico (§7.9). Traducciones iniciales; revisar en la
## pasada de i18n (# TODO(G): review es).


# Plano de informacion del concepto (Direccion B, "concepto primero").
# El concepto es un objeto de primer nivel con su propio enlace permanente en
# /ui/hts/concepts?system=...&code=..., con tres paneles: Identidad,
# Correspondencias (en todos los ConceptMap almacenados) y Subsuncion.
hts-concept-title = Concepto
hts-concept-lede = Un código, visto desde todos los ángulos que el servidor de terminología puede responder: qué es, con qué se corresponde y dónde se sitúa en la jerarquía.
hts-concept-open = Abrir concepto
hts-concept-panel-loading = Cargando
hts-concept-panel-open = Abrir este panel

hts-concept-identity-heading = Identidad
hts-concept-status-active = Activo
hts-concept-status-inactive = Inactivo
hts-concept-status-unreported = Actividad no informada
hts-concept-field-system = Sistema
hts-concept-field-code = Código
hts-concept-field-display = Denominación
hts-concept-field-code-system-name = Nombre del CodeSystem
hts-concept-field-version = Versión
hts-concept-field-selectability = Seleccionabilidad
hts-concept-selectability-abstract = Abstracto (no seleccionable)
hts-concept-selectability-selectable = Seleccionable
hts-concept-field-definition = Definición
hts-concept-field-neighbours = Vecinos en la jerarquía
hts-concept-field-used-supplements = Suplementos aplicados
hts-concept-designations-heading = Designaciones
hts-concept-designations-value = Designación
hts-concept-designations-language = Idioma
hts-concept-designations-use = Uso
hts-concept-properties-heading = Propiedades
hts-concept-properties-code = Propiedad
hts-concept-properties-value = Valor
hts-concept-raw-response = Respuesta original

hts-concept-mappings-heading = Correspondencias
hts-concept-mappings-direction-forward = Correspondencias en las que este concepto es el origen, en todos los ConceptMap almacenados.
hts-concept-mappings-direction-reverse = Correspondencias en las que este concepto es el destino, en todos los ConceptMap almacenados.
hts-concept-mappings-switch-forward = Mostrar correspondencias desde este concepto
hts-concept-mappings-switch-reverse = Mostrar correspondencias hacia este concepto
hts-concept-mappings-empty = Ningún ConceptMap corresponde a este concepto.
hts-concept-mappings-vocabulary = Vocabulario de correspondencia
hts-concept-mappings-vocabulary-equivalence = equivalence (R4 / R4B)
hts-concept-mappings-vocabulary-relationship = relationship (R5 / R6)
hts-concept-mappings-vocabulary-unknown = No informado
hts-concept-mappings-unattributable = El servidor no atribuye las coincidencias en modo inverso a un mapa de origen, por lo que no se puede mostrar la procedencia. Cambie a la dirección directa para ver de qué ConceptMap procede cada correspondencia.
hts-concept-mappings-origin = Mapa de origen
hts-concept-mappings-column-code = Código
hts-concept-mappings-column-system = Sistema
hts-concept-mappings-column-display = Denominación
hts-concept-mappings-column-mapping = Relación

hts-concept-relations-heading = Subsunción
hts-concept-relations-lede = Cada fila es una comprobación de subsunción. El candidato a ancestro siempre se envía como código A, de modo que una jerarquía coherente responde siempre "subsumes".
hts-concept-relation-parent = Padre
hts-concept-relation-child = Hijo
hts-concept-relation-manual = Comparado
hts-concept-relations-column-relation = Relación
hts-concept-relations-column-question = Pregunta formulada
hts-concept-relations-column-outcome = Resultado
hts-concept-relations-subsumes-verb = subsume a
hts-concept-subsumes-outcome-equivalent = Equivalente
hts-concept-subsumes-outcome-subsumes = Subsume
hts-concept-subsumes-outcome-subsumed-by = Subsumido por
hts-concept-subsumes-outcome-not-subsumed = Sin subsunción
hts-concept-relations-conflict-caveat = La consulta del concepto informa de este vínculo jerárquico, pero la comprobación de subsunción no lo confirma. Suele significar que el cierre de subsunción no se reconstruyó tras reimportar el CodeSystem; la jerarquía en sí se conservó.
hts-concept-relations-empty = Este concepto no tiene padres ni hijos que comparar.
hts-concept-relations-dropped = No se comprobaron { $n } comparadores adicionales; este panel ejecuta como máximo 20 comprobaciones de subsunción por renderizado.
hts-concept-relations-compare-label = Comparar con el código
hts-concept-relations-compare-placeholder = Otro código de este sistema
hts-concept-relations-compare-hint = El sistema queda fijado al de este concepto, así que introduzca solo el código. La comprobación pregunta si ese código subsume a este.
hts-concept-relations-compare-submit = Comparar

## Páginas de detalle de HTS -- cabecera compacta V3 (#551, capas B/C/D).
## Etiquetas compartidas de la fila de chips y del desplegable de las
## páginas de detalle de CodeSystem / ValueSet / ConceptMap, además de los
## títulos de los paneles de resultado y las dos notas de honestidad
## (paginador en modo árbol, originMap en modo inverso).

hts-detail-facts-label = Datos
hts-detail-canonical-url = URL canónica
hts-detail-version-label = Versión
hts-detail-status-label = Estado
hts-cs-detail-facts-summary = Todos los datos del CodeSystem
hts-vs-detail-facts-summary = Todos los datos del ValueSet
hts-cm-detail-facts-summary = Todos los datos del ConceptMap
hts-cs-lookup-definition = Definición
hts-cs-validate-result-heading = Resultado de la validación
hts-cs-subsumes-result-heading = Resultado de la subsunción
hts-vs-expand-result-heading = Expansión
hts-vs-expand-table-caption = Miembros de la expansión devueltos por el servidor de terminología.
hts-vs-expand-tree-note = El modo árbol devuelve toda la jerarquía; el paginador solo existe en modo plano.
hts-cm-translate-table-caption = Coincidencias de traducción devueltas por el servidor de terminología.
hts-cm-translate-origin-reverse-note = En modo inverso HTS omite originMap, por lo que una coincidencia no puede atribuirse a un mapa de conceptos concreto. Cada celda de Origen queda como raya por diseño: no es un valor ausente.


# Capability & Conformance page (HTS mirror of HFS's page). The shared
# `cap-*` and `nav-capability-conformance` keys carry everything both
# pages say identically; only what is specific to a terminology server
# lives here.
hts-capability-lede = Lo que este servidor de terminología anuncia ahora mismo — compuesto en vivo desde /metadata.
hts-capability-operations-empty = No se anuncian operaciones.
hts-capability-rest-empty = No se anuncian recursos REST.
hts-capability-terminology-heading = Capacidades de terminología
hts-capability-expansion-hierarchical = Expansión jerárquica
hts-capability-expansion-paging = Paginación de la expansión
hts-capability-expansion-incomplete = Expansiones incompletas
hts-capability-expansion-parameters = Parámetros de $expand
hts-capability-validate-code-translations = Traducciones en validate-code
hts-capability-translation-needs-map = La traducción requiere un mapa
hts-capability-closure = Mantenimiento de cierre
hts-capability-code-systems-declared = Sistemas de códigos declarados
hts-capability-flag-true = Sí
hts-capability-flag-false = No

# Home V3 tile sub-lines. The mockup folds Backend, FHIR version,
# Bundled data and Avg latency into the sub-line of the tile each
# qualifies, instead of giving them tiles of their own.
hts-home-tile-status-sub = backend { $backend } · FHIR { $version }
hts-home-tile-uptime-sub = hts v{ $version } · sin reinicios desde las { $since } UTC
hts-home-tile-uptime-sub-noclock = hts v{ $version }
hts-home-tile-loaded-systems-sub = { $mib } MiB empaquetados en disco
hts-home-tile-requests-sub = { $ms } ms de media · desde /metrics

# Chart caption, composed from the SELECTED window and status class.
# Each locale owns its own word order through the two placeables.
hts-home-chart-hint = { $window }, { $classes }. Se muestrea mientras esta página está abierta. Excluye el refresco propio de 15 s y las lecturas de /metrics.
hts-home-chart-hint-window-15m = Últimos 15 minutos
hts-home-chart-hint-window-1h = Última hora
hts-home-chart-hint-window-6h = Últimas 6 horas
hts-home-chart-hint-series-all = todas las clases de estado
hts-home-chart-hint-series-2xx = solo respuestas 2xx
hts-home-chart-hint-series-4xx = solo respuestas 4xx
hts-home-chart-hint-series-5xx = solo respuestas 5xx

## Autenticación solo con bearer (#1560): auth activa, sin inicio de sesión de navegador
auth-bearer-only = La autenticación está habilitada en este servidor, pero no hay un inicio de sesión de navegador configurado. Las páginas que llaman a la API FHIR desde el navegador — Recursos, Batch / Transaction, consultas guardadas y preferencias — se rechazan con 401. Configura HFS_UI_LOGIN_CLIENT_ID para habilitar el inicio de sesión, o usa la API directamente con un token bearer.
batch-sign-in-required = Este servidor no tiene un inicio de sesión de navegador configurado, así que esta página no puede autenticar la petición. Configura HFS_UI_LOGIN_CLIENT_ID para ejecutar bundles desde aquí.
batch-too-large = El bundle supera el tamaño que acepta este servidor. Divídelo en bundles más pequeños o aumenta HFS_MAX_BODY_SIZE en el servidor.

## Batch / Transaction: modo pegar y comprobaciones en el cliente (p1-ui)
batch-mode-label = Entrada del Bundle
batch-mode-file = Subir archivo
batch-mode-paste = Pegar JSON
batch-paste-label = JSON del Bundle
batch-paste-placeholder = Pegue aquí un Bundle de tipo batch o transaction…
batch-paste-hint = Se comprueba mientras escribe; cada problema indica su línea y columna.
batch-format = Formatear
batch-format-failed = Corrija el error de sintaxis JSON antes de formatear.
batch-continue = Revisar el plan
batch-check-ok = No se encontraron problemas.
batch-check-errors = Errores
batch-check-warnings = Advertencias
batch-check-by-method = Por método
batch-check-by-type = Por tipo de recurso
batch-check-entries = %count% entradas en total
batch-check-blocked = Corrija los errores para continuar. Las advertencias no bloquean.
batch-check-at = línea %line%, col. %col%

## Comprobaciones de Bundle compartidas (bundle-check.js). El navegador rellena los marcadores %name%.
bundle-check-empty = Aún no hay nada que comprobar: pegue o suba un Bundle.
bundle-check-json-syntax = Error de sintaxis JSON: %detail%.
bundle-check-json-eof = el texto termina antes de que el JSON esté completo
bundle-check-json-unexpected = carácter inesperado %char%
bundle-check-json-expected = se esperaba %expected%
bundle-check-not-object = El JSON debe ser un único objeto de recurso.
bundle-check-not-bundle = resourceType es «%found%»; un batch o una transaction debe ser un Bundle.
bundle-check-missing-type = Falta Bundle.type: establézcalo en batch o transaction.
bundle-check-bad-type = Bundle.type es «%found%». Solo se pueden enviar Bundles batch y transaction, porque solo ellos llevan una petición (method y url) en cada entrada.
bundle-check-entry-not-array = Bundle.entry debe ser un arreglo.
bundle-check-no-entries = El Bundle no tiene entradas, así que no se ejecutaría nada.
bundle-check-entry-not-object = La entrada %entry% no es un objeto JSON.
bundle-check-no-request = La entrada %entry% no tiene request: cada entrada de batch o transaction necesita request.method y request.url.
bundle-check-no-method = Entrada %entry%: falta request.method.
bundle-check-bad-method = Entrada %entry%: request.method «%method%» no es GET, POST, PUT, PATCH, DELETE ni HEAD.
bundle-check-no-url = Entrada %entry%: falta request.url.
bundle-check-bad-url = Entrada %entry%: una request.url de %method% debe tener la forma Type/id (o Type?search para un %method% condicional), no «%url%».
bundle-check-no-resource = Entrada %entry%: una entrada %method% necesita un resource.
bundle-check-no-resource-type = Entrada %entry%: el recurso no tiene resourceType.
bundle-check-url-type-mismatch = Entrada %entry%: request.url apunta a %url_type%, pero el recurso es %type%.
bundle-check-id-mismatch = Entrada %entry%: request.url indica el id «%url_id%», pero resource.id es «%id%».
bundle-check-duplicate-full-url = Entrada %entry%: la fullUrl %fullurl% también la usa la entrada %other%.
bundle-check-unresolved-urn = Entrada %entry%: la referencia %ref% no coincide con la fullUrl de ninguna entrada de esta transacción.
bundle-check-too-large = El Bundle ocupa %size%, más de lo que acepta este servidor (%limit%). Divídalo o aumente HFS_MAX_BODY_SIZE en el servidor.

## Validar sin importar ($validate): página Batch y página Validar
nav-validate = Validar
validate-run = Validar
validate-run-hint = Comprueba el Bundle en el servidor ($validate) sin importar nada.
validate-per-entry = Validar cada entrada por separado
validate-per-entry-hint = Además ejecuta $validate sobre el recurso de cada entrada, contra su propio meta.profile.
validate-running = Validando…
validate-progress = Validando la entrada %done% de %total%…
validate-result-heading = Resultado de la validación
validate-heading-bundle = Bundle
validate-heading-resource = Recurso
validate-heading-entries = Entradas validadas por separado
validate-verdict-ok = Válido: sin errores ni advertencias
validate-verdict-errors = Errores: %errors% · Advertencias: %warnings%
validate-verdict-warnings = Válido, con advertencias: %warnings%
validate-group-error = Errores
validate-group-warning = Advertencias
validate-group-information = Información
validate-sev-fatal = Fatal
validate-sev-error = Error
validate-sev-warning = Advertencia
validate-sev-information = Información
validate-col-severity = Gravedad
validate-col-location = Ubicación
validate-col-code = Código
validate-col-message = Mensaje
validate-entry-label = entrada %n% (%name%)
validate-entry-label-bare = entrada %n%
validate-request-failed = La petición de validación falló (%detail%).
validate-page-heading = Validar
validate-page-lede = Compruebe cualquier recurso o Bundle contra las reglas FHIR y los perfiles que conoce este servidor, sin guardar nada.
validate-input = Recurso
validate-input-label = JSON del recurso o Bundle
validate-input-placeholder = Pegue un recurso FHIR o un Bundle…
validate-profile = Perfil (opcional)
validate-profile-placeholder = p. ej. https://twcore.mohw.gov.tw/ig/twcore/StructureDefinition/Patient-twcore
validate-profile-hint = Una URL canónica de StructureDefinition; la lista ofrece los perfiles guardados en este servidor. El meta.profile propio del recurso siempre se comprueba también.
validate-profiles-loaded = %count% perfiles disponibles

## FHIR package (IG) management (/ui/packages)
nav-packages = Paquetes IG
packages-title = Paquetes IG
packages-lede = Instale, actualice y elimine guías de implementación FHIR (paquetes npm). Los perfiles y demás recursos de conformidad van a este servidor; la terminología, al servidor de terminología.
packages-disabled = La administración de paquetes está desactivada. Configure HFS_ADMIN_UI=true en el servidor para instalar y eliminar paquetes aquí.
packages-back = Volver a Paquetes IG
packages-tabs-label = Vistas de paquetes
packages-tab-installed = Instalados
packages-tab-market = Explorar el catálogo
packages-tab-upload = Subir .tgz
packages-status-registry = Registro
packages-status-terminology = Servidor de terminología
packages-status-cache = Caché de paquetes
packages-registry-public = packages.fhir.org (packages2.fhir.org como respaldo)
packages-registry-mirror = Réplica: %urls%
packages-registry-off = Desactivado — solo paquetes subidos o en caché
packages-terminology-none = No configurado — la terminología se guarda en este servidor
packages-installed-search = Filtrar paquetes instalados…
packages-col-name = Paquete
packages-col-version = Versión
packages-col-fhir = FHIR
packages-col-installed = Instalado
packages-col-resources = Recursos
packages-col-dependencies = Dependencias
packages-col-status = Estado
packages-installed-empty = Aún no hay paquetes instalados. Explore el catálogo o suba un .tgz.
packages-installed-count = Paquetes instalados: %count%
packages-state-installed = instalado
packages-state-installing = instalando
packages-state-failed = fallido
packages-state-cancelled = cancelado
packages-explicit = solicitado
packages-dependency-of = dependencia
packages-jobs-heading = Trabajos recientes
packages-jobs-empty = No hay trabajos de paquetes desde que se inició el servidor.
packages-job-plan = plan
packages-job-install = instalación
packages-job-uninstall = eliminación
packages-job-queued = en cola
packages-job-running = en curso
packages-job-succeeded = terminado
packages-job-completed-with-errors = terminado con errores
packages-job-failed = fallido
packages-job-cancelled = detenido
packages-market-search = Buscar guías por nombre, título o editor…
packages-filter-category = Categoría
packages-filter-country = País
packages-filter-authority = Editor
packages-filter-fhir = Versión de FHIR
packages-filter-any = Cualquiera
packages-market-count = Guías: %count%
packages-market-unavailable = El índice del catálogo no está disponible: %detail%
packages-market-empty = Ninguna guía coincide con estos filtros.
packages-market-more = Mostrar más
packages-latest = Última %version%
packages-installed-versions = Instalado: %versions%
packages-install = Instalar…
packages-install-by-name = Instalar por nombre de paquete
packages-install-name = Nombre del paquete
packages-install-name-placeholder = p. ej. hl7.fhir.us.core
packages-install-version = Versión
packages-install-version-latest = Última
packages-install-version-hint = Las versiones provienen del registro; con el registro desactivado, de la caché de paquetes.
packages-registry-results = Otros paquetes del registro
packages-registry-results-empty = No coinciden otros paquetes del registro.
packages-upload-drop = Suelte aquí un paquete FHIR (.tgz)
packages-upload-browse = o haga clic para elegir un archivo
packages-upload-hint = Funciona sin conexión: se comprueba el archivo (rutas, tamaños, manifiesto) y se guarda en la caché de paquetes del servidor. Con el registro desactivado, suba también las dependencias u omítalas en el plan.
packages-upload-reading = Subiendo %name%…
packages-upload-failed = Error al subir: %detail%
packages-wizard-heading = Instalar %package%
packages-wizard-steps = Pasos de la instalación
packages-step-tree = Dependencias
packages-step-options = Opciones
packages-step-progress = Progreso
packages-step-result = Resultado
packages-wizard-planning = Resolviendo dependencias y leyendo paquetes…
packages-tree-lede = El paquete y todo lo que necesita. Las definiciones básicas de FHIR vienen integradas en este servidor y nunca se importan.
packages-action-install = instalar
packages-action-reinstall = reinstalar
packages-action-upgrade = actualizar desde %from%
packages-action-satisfied = ya instalado (%by%)
packages-action-embedded = integrado en el servidor
packages-action-skipped = omitido
packages-action-missing = no encontrado
packages-skip-dependency = Omitir
packages-counts-hfs = %count% recursos a este servidor
packages-counts-hts = %count% al servidor de terminología
packages-counts-examples = %count% ejemplos omitidos
packages-conflict-adopts = %count% ya guardados, idénticos (adoptados)
packages-conflict-overwrites-unmanaged = %count% sobrescriben un recurso sin paquete (se conserva al eliminar)
packages-conflict-shared = %count% compartidos con otro paquete
packages-conflict-replaces-version = %count% reemplazan la versión instalada
packages-conflict-renamed = %count% reciben un id con sufijo de versión
packages-conflict-renamed-clash = %count% comparten el id con otro recurso y reciben un id nuevo
packages-plan-totals = Paquetes a instalar: %packages% · recursos a este servidor: %hfs% · al servidor de terminología: %hts%
packages-plan-warning = Nota: %detail%
packages-plan-failed = No se pudo preparar el plan: %detail%
packages-plan-package-error = %package%: %detail%
packages-opt-examples = Incluir recursos de ejemplo
packages-opt-examples-hint = Los ejemplos (package/example/ o marcados como ejemplo por la IG) no se importan por defecto.
packages-opt-dependencies = Instalar dependencias
packages-opt-mode = Si hay otra versión instalada
packages-opt-mode-upgrade = Actualizar: la nueva versión la reemplaza
packages-opt-mode-side = En paralelo: conservar ambas (los id en conflicto reciben un sufijo de versión)
packages-opt-policy = Versiones de las dependencias
packages-opt-policy-newer = Una versión más reciente instalada satisface la dependencia (recomendado)
packages-opt-policy-exact = Instalar exactamente las versiones que indica cada paquete
packages-opt-terminology = Terminología (CodeSystem, ValueSet, ConceptMap)
packages-opt-terminology-auto = El servidor de terminología (este servidor si no hay ninguno)
packages-opt-terminology-hfs = Solo este servidor
packages-opt-terminology-both = Ambos
packages-replan = Actualizar el plan
packages-next = Siguiente
packages-back-step = Atrás
packages-start-install = Instalar
packages-cancel = Cancelar
packages-stop = Detener
packages-close = Cerrar
packages-progress-packages = Paquete %done% de %total%
packages-progress-resources = %done% de %total% recursos
packages-col-type = Tipo de recurso
packages-col-written = Escritos
packages-col-failed = Fallidos
packages-col-skipped = Omitidos
packages-log = Registro
packages-result-ok = Instalación terminada sin errores. Paquetes instalados: %count%.
packages-result-errors = Instalación terminada. Paquetes instalados: %count% · errores: %errors%.
packages-result-failed = La instalación falló: %detail%
packages-result-cancelled = Detenido. Lo escrito hasta ahora queda registrado en el paquete; elimínelo para limpiar.
packages-errors-heading = Errores
packages-col-package = Paquete
packages-col-resource = Recurso
packages-col-message = Mensaje
packages-download-report = Descargar informe (JSON)
packages-view-installed = Ver paquetes instalados
packages-reindex-hint = Se instalaron %count% SearchParameter. Los datos existentes se podrán buscar con ellos tras reindexar.
packages-reindex = Iniciar $reindex
packages-reindex-started = Reindexación iniciada.
packages-request-failed = La solicitud falló: %detail%
packages-sign-in = Inicie sesión con una cuenta de administrador para gestionar paquetes.
packages-detail-loading = Cargando…
packages-detail-not-installed = %package% no está instalado.
packages-detail-meta = Paquete
packages-meta-title = Título
packages-meta-canonical = Canonical
packages-meta-fhir = Versiones de FHIR
packages-meta-source = Origen
packages-meta-sha1 = SHA-1
packages-meta-verified = verificado con el registro
packages-meta-installed-at = Instalado
packages-meta-requested-by = Solicitado por
packages-meta-status = Estado
packages-detail-dependencies = Dependencias
packages-detail-no-dependencies = Sin dependencias.
packages-detail-dependents = Requerido por
packages-detail-no-dependents = Ningún paquete instalado depende de este.
packages-dep-embedded = integrado
packages-dep-skipped = omitido
packages-dep-missing = no instalado
packages-detail-resources = Recursos
packages-detail-resources-count = Recursos: %count%
packages-target-hfs = este servidor
packages-target-hts = servidor de terminología
packages-res-shared = también en %packages%
packages-res-preexisting = existía antes de la instalación (se conserva al eliminar)
packages-res-open = Abrir
packages-res-browse-type = Explorar todos los %type%
packages-col-id = Id
packages-col-url = Canonical
packages-col-where = Guardado en
packages-col-note = Nota
packages-upgrade = Actualizar…
packages-upgrade-none = Esta es la versión publicada más reciente.
packages-upgrade-to = Actualizar a %version%
packages-remove = Eliminar…
packages-remove-title = Eliminar %package%
packages-remove-lede = Borra los recursos que solo trajo este paquete, da de baja sus perfiles y elimina su terminología del servidor de terminología. Los recursos compartidos con otros paquetes, o que ya existían antes de la instalación, se conservan.
packages-remove-blocked = No se puede eliminar mientras %packages% dependan de él.
packages-remove-confirm = Escriba %name% para confirmar
packages-remove-submit = Eliminar paquete
packages-removing = Eliminando…
packages-removed = Eliminado: %deleted% recursos borrados, %kept% conservados.


## Terminology management (HTS_ADMIN_UI): inventory, delete, jobs, archive import
hts-nav-terminology = Terminología
hts-nav-jobs = Tareas
hts-admin-disabled-title = La gestión de terminología está desactivada
hts-admin-disabled-body = Subir archivos, importar en segundo plano, eliminar versiones y la lista de tareas necesitan la API de administración, que este servidor no tiene activada. Un administrador puede activarla con la variable de entorno siguiente y un reinicio; mantén /admin y /ui/hts accesibles solo para administradores.
hts-admin-request-failed = La solicitud falló:
hts-term-title = Inventario de terminología
hts-term-lede = Cada versión de CodeSystem almacenada, con su tamaño, su origen y qué versión responde a las solicitudes que no indican versión.
hts-term-import-action = Importar terminología
hts-term-summary = Resumen de la base de datos
hts-term-stat-db = Tamaño de la base de datos
hts-term-stat-reclaimable = { $size } recuperables al compactar
hts-term-stat-systems = Sistemas de códigos
hts-term-stat-systems-all = todas las filas almacenadas
hts-term-stat-systems-populated = con conceptos
hts-term-stat-concepts = Conceptos
hts-term-stat-counted = contados { $when }
hts-term-stat-other = ValueSets / ConceptMaps
hts-term-stat-other-sub = recursos almacenados
hts-term-stats-pending = Contando conceptos: en una base de datos grande el primer recuento tarda un poco. La tabla se actualiza sola.
hts-term-filter = Filtrar sistemas de códigos
hts-term-filter-placeholder = Filtrar por URL, nombre o título
hts-term-sort-size = Más grandes primero
hts-term-show-all = Incluir esbozos sin conceptos
hts-term-filter-apply = Aplicar
hts-term-table-caption = Versiones de CodeSystem almacenadas
hts-term-col-system = Sistema de códigos
hts-term-col-version = Versión
hts-term-col-concepts = Conceptos
hts-term-col-size = Tamaño
hts-term-col-imported = Importado
hts-term-col-source = Archivo de origen
hts-term-col-current = Actual
hts-term-col-actions = Acciones
hts-term-empty = Ningún sistema de códigos coincide.
hts-term-supplement = suplemento
hts-term-current = actual
hts-term-pinned = fijada
hts-term-unpin = Quitar fijación
hts-term-set-current = Fijar como actual
hts-term-delete = Eliminar…
hts-term-other-heading = Eliminar un ValueSet o ConceptMap
hts-term-other-help = Elimina una versión de ValueSet o un ConceptMap almacenado por su URL canónica. La eliminación se ejecuta como tarea en segundo plano.
hts-term-other-type = Tipo de recurso
hts-term-other-url = URL canónica
hts-term-other-delete = Eliminar
hts-term-maint-heading = Mantenimiento de la base de datos
hts-term-compact-warning = Eliminar terminología no reduce el archivo SQLite. La compactación (VACUUM) lo reescribe y devuelve el espacio libre al disco, pero bloquea cualquier otra escritura mientras dura —minutos en una base de varios gigabytes— y necesita espacio libre de aproximadamente el doble del tamaño de la base. Ejecútala en una ventana de mantenimiento.
hts-term-compact = Compactar la base de datos
hts-term-notice-pinned = La versión fijada responde ahora a las solicitudes sin versión.
hts-term-notice-unpinned = Se quitó la fijación; la versión más reciente vuelve a ser la actual.
hts-del-title = Eliminar una versión de CodeSystem
hts-del-back = ← Inventario de terminología
hts-del-confirm-mismatch = Escribe la URL canónica exacta para confirmar la eliminación.
hts-del-what = Qué se eliminará
hts-del-rows = Se elimina también
hts-del-rows-value = conceptos, designaciones, propiedades, jerarquía, clausura transitiva, filas del índice de texto completo y expansiones en caché de esta versión
hts-del-after = Después
hts-del-after-none = No queda ninguna versión de este sistema de códigos; sus solicitudes fallarán.
hts-del-after-version = Las solicitudes sin versión usarán la versión { $version }.
hts-del-pinned-note = Esta versión está fijada como actual; al eliminarla se quita la fijación.
hts-del-deps = { $n ->
    [one] 1 recurso dependiente
   *[other] { $n } recursos dependientes
}
hts-del-deps-none = Ningún ValueSet, suplemento ni ConceptMap hace referencia a esta versión.
hts-del-deps-warning = Estos recursos hacen referencia al sistema de códigos. Los que fijan esta versión dejan de funcionar; los demás pasan a la versión actual restante.
hts-del-dep-type = Tipo
hts-del-dep-binding = Referencia
hts-del-dep-pinned = fija esta versión
hts-del-dep-floating = cualquier versión
hts-del-confirm-heading = Confirmar
hts-del-confirm-label = URL canónica del sistema de códigos
hts-del-confirm-hint = La eliminación se ejecuta como tarea en segundo plano y no se puede deshacer.
hts-del-cancel = Cancelar
hts-del-submit = Eliminar esta versión
hts-jobs-lede = Las importaciones, eliminaciones y tareas de mantenimiento se ejecutan en segundo plano, de una en una; una tarea enviada mientras otra se ejecuta espera en la cola.
hts-jobs-empty = Todavía no hay tareas.
hts-job-title = Tarea
hts-job-back = ← Tareas
hts-job-not-found = Esta tarea no existe.
hts-job-notice-cancel-requested = Cancelación solicitada. Una importación en curso se detiene en el siguiente lote y elimina lo que creó.
hts-job-progress = Progreso
hts-job-cancel = Cancelar tarea
hts-job-phases = Fases
hts-job-requested-by = Solicitada por
hts-job-created = Enviada
hts-job-started = Iniciada
hts-job-finished = Terminada
hts-job-log = Registro
hts-job-log-empty = Aún no hay registro.
hts-job-result-raw = Resultado sin procesar
hts-job-queued-position = En cola: posición { $n }
hts-job-concepts = { $n } conceptos
hts-job-created-by = por { $who }
hts-job-batches = lote { $done } de { $total }
hts-job-batches-open = lote { $done }
hts-job-rows = { $done } de { $total } filas
hts-job-result-code-systems = CodeSystems
hts-job-result-value-sets = ValueSets
hts-job-result-concept-maps = ConceptMaps
hts-job-result-concepts = Conceptos
hts-job-result-deleted-concepts = Conceptos eliminados
hts-job-result-growth = Crecimiento de la base de datos
hts-job-result-reclaimed = Espacio recuperado
hts-job-result-elapsed = Duración
hts-job-status-queued = En cola
hts-job-status-running = En curso
hts-job-status-succeeded = Completada
hts-job-status-failed = Fallida
hts-job-status-cancelled = Cancelada
hts-job-status-interrupted = Interrumpida
hts-job-phase-queued = en cola
hts-job-phase-running = en curso
hts-job-phase-prepare = preparación
hts-job-phase-download = descarga
hts-job-phase-parse = análisis
hts-job-phase-write = escritura
hts-job-phase-closure = clausura jerárquica
hts-job-phase-fts = índice de texto completo
hts-job-phase-analyze = estadísticas
hts-job-phase-warm = precarga de cachés
hts-job-phase-hierarchy = jerarquía
hts-job-phase-concepts = conceptos
hts-job-phase-caches = cachés
hts-job-phase-delete = eliminación
hts-job-phase-vacuum = compactación
hts-job-phase-done = terminado
hts-job-phase-succeeded = terminado
hts-job-phase-failed = fallido
hts-job-phase-cancelled = cancelado
hts-job-phase-interrupted = interrumpido
hts-reg-none = Ningún paquete coincide.
hts-reg-pick = Usar
hts-reg-search = Buscar en el registro
hts-reg-package = Paquete
hts-reg-version = Versión
hts-reg-latest = última
hts-import-help-admin = Carga terminología desde un archivo de publicación (SNOMED CT RF2, LOINC, ICD-10-CM…), un archivo del servidor, un paquete FHIR o un Bundle pegado.
hts-import-archive-heading = Archivo de terminología
hts-import-bundle-section = Bundle FHIR
hts-import-licence-title = Contenido con licencia
hts-import-licence-body = SNOMED CT y LOINC (y RxNorm) tienen licencia: obtén los archivos de publicación con tu propia licencia y súbelos o colócalos en el directorio de importación del servidor. Este servidor nunca los descarga. Los formatos marcados con * en la lista tienen licencia.
hts-import-src-upload = Subir un archivo
hts-import-src-dir = Directorio de importación del servidor
hts-import-src-registry = Registro de paquetes
hts-import-options = Opciones de importación
hts-import-format-auto = Detectar a partir del archivo
hts-import-version-override = Versión (sustituir)
hts-import-languages = Idiomas de traducción
hts-import-properties = Propiedades LOINC a conservar
hts-import-options-hint = Deja los campos vacíos para importarlo todo tal como se publicó. Sustituir la versión solo se aplica a archivos de un único sistema; los idiomas limitan las traducciones de SNOMED CT y LOINC; la lista de propiedades LOINC conserva solo esas propiedades.
hts-import-job-note = La importación se ejecuta como tarea en segundo plano; la página siguiente muestra su progreso.
hts-import-start-job = Iniciar importación
hts-up-needs-js = Subir archivos requiere JavaScript. Sin él, copia el archivo al directorio de importación del servidor.
hts-up-drop = Suelta aquí un archivo de terminología
hts-up-drop-hint = o haz clic para elegirlo —hasta { $max }; los archivos grandes se envían por partes y se reanudan tras una interrupción
hts-up-pause = Pausar
hts-up-resume = Reanudar
hts-up-format = Formato
hts-up-growth = Crecimiento estimado de la base de datos
hts-up-incomplete = Subidas sin terminar
hts-up-incomplete-hint = Elige de nuevo el mismo archivo para continuar donde se detuvo.
hts-up-uploaded = Archivos subidos
hts-up-none = Todavía no hay archivos subidos.
hts-up-uploading = Subiendo {"{name}"}: {"{done}"} de {"{total}"} ({"{pct}"} %) · {"{speed}"}/s · faltan {"{eta}"}
hts-up-paused = En pausa al {"{pct}"} %. Reanuda o elige el archivo de nuevo más tarde.
hts-up-resumed = Reanudando {"{name}"} al {"{pct}"} %
hts-up-verifying = Se recibieron todas las partes; calculando la suma de verificación e inspeccionando el archivo…
hts-up-done = {"{name}"} subido.
hts-up-failed = La subida se detuvo: {"{error}"}. Se reanuda al elegir de nuevo el mismo archivo.
hts-up-detected = Detectado {"{format}"}
hts-up-undetected = No se pudo detectar el formato; elígelo en Opciones de importación.
hts-up-space = No hay espacio libre suficiente para esta importación: {"{free}"} libres, se necesitan {"{need}"}.
hts-dir-empty = El directorio está vacío.
hts-dir-file = Archivo
hts-dir-modified = Modificado
hts-dir-detect-on-import = se detecta al importar
hts-term-showing = Se muestran { $shown } de { $total } sistemas de códigos
hts-term-show-all-rows = Mostrar todos

## Administrator sign-in (HFS_ADMIN_PASSWORD / HTS_ADMIN_PASSWORD), shared by
## both consoles: /ui/admin/login and /ui/hts/login.
admin-login-title = Inicio de sesión de administrador
admin-login-lede = Instalar paquetes, gestionar inquilinos, importar terminología y las demás páginas de administración requieren la contraseña de administrador.
admin-login-user = Nombre de usuario
admin-login-password = Contraseña
admin-login-submit = Iniciar sesión
admin-login-invalid = Nombre de usuario o contraseña incorrectos.
admin-login-throttled = Demasiados intentos fallidos desde su dirección. Vuelva a intentarlo en { $minutes } minutos.
admin-login-cross-site = El formulario de inicio de sesión debe enviarse desde este sitio. Recargue la página e inténtelo de nuevo.
admin-login-locked = La administración está bloqueada: este servidor no tiene contraseña de administrador. Defina { $var } (o { $var }_FILE) y reinícielo.
admin-user-hint = Administrador
